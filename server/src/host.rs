use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use protocol::types::{
    AgentClosedPayload, AgentCompactNotifyPayload, AgentCompactPayload, AgentCompactStatus,
    TeamCompactNotifyPayload, TeamCompactPayload, TeamCompactStatus,
};
use protocol::{
    AgentControlStatus, AgentId, AgentInput, AgentOrigin, AgentStartPayload, BackendSetupPayload,
    BrowseBootstrapListing, BrowseBootstrapPayload, CustomAgent, CustomAgentDeletePayload,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, FrameKind, GitBranchName, HostAbsPath,
    HostBootstrapPayload, HostBrowseInitial, HostBrowseListPayload, HostBrowseStartPayload,
    HostSettingsPayload, ImageData, McpServerConfig, McpServerDeletePayload, McpServerId,
    McpServerNotifyPayload, McpServerUpsertPayload, McpTransportConfig, MobileDeviceRenamePayload,
    MobileDeviceRevokePayload, MobilePairingCancelPayload, NewAgentPayload, Project,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload,
    ProjectDiscardFilePayload, ProjectGitCommitPayload, ProjectGitCommitResultPayload, ProjectId,
    ProjectListDirPayload, ProjectNotifyPayload, ProjectPath, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRenamePayload, ProjectReorderPayload, ProjectRootPath,
    ProjectSearchCancelPayload, ProjectSearchCompletePayload, ProjectSearchFileResult,
    ProjectSearchPayload, ProjectSearchResultsPayload, ProjectSource, ProjectStageFilePayload,
    ProjectStageHunkPayload, ProjectUnstageFilePayload, ReviewActionPayload, ReviewCreatePayload,
    ReviewDiffSelection, ReviewId, ReviewSubmitTarget, RunBackendSetupPayload, SendMessagePayload,
    SessionId, SessionListPayload, SessionSchemaEntry, SessionSchemasPayload,
    SessionSettingsSchema, SessionSummary, SetSettingPayload, Skill, SkillNotifyPayload,
    SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload, SteeringDeletePayload,
    SteeringNotifyPayload, SteeringScope, SteeringUpsertPayload, StreamPath, TeamCreatePayload,
    TeamDeletePayload, TeamDraftApplyTemplatePayload, TeamDraftCommitPayload,
    TeamDraftCreatePayload, TeamDraftDiscardPayload, TeamDraftNotifyPayload,
    TeamDraftShufflePayload, TeamDraftUpdatePayload, TeamId, TeamMember,
    TeamMemberBindingNotifyPayload, TeamMemberCreatePayload, TeamMemberDeletePayload, TeamMemberId,
    TeamMemberNotifyPayload, TeamMemberRole, TeamMemberShufflePayload,
    TeamMemberShuffleSuggestionNotifyPayload, TeamMemberState, TeamMemberUpdatePayload,
    TeamNotifyPayload, TeamRenamePayload, TeamSetManagerPayload, TerminalCreatePayload, TerminalId,
    TerminalLaunchTarget, TerminalResizePayload, TerminalSendPayload, WorkbenchCreatePayload,
    WorkbenchRemovePayload, WorkbenchRoot,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::agent::customization::{
    ResolveSpawnConfigRequest, ResolvedSpawnConfig, protocol_mcp_servers_to_startup,
    resolve_spawn_config,
};
use crate::agent::registry::{
    AgentRegistry, AgentStartupFailure, InitialAgentAlias, InitialAgentAliasPersistence,
    RelaySpawnRequest, ResolvedSpawnRequest,
};
use crate::agent::{
    AgentHandle, CompactionStart, CompactionSummary, DEFAULT_COMPACTION_SUMMARY_MAX_BYTES,
    GenerateAgentNameRequest, InterruptOutcome, MAX_COMPACTION_SUMMARY_BYTES, derive_agent_name,
    generate_agent_name,
};
use crate::agent_control_mcp::AgentControlMcpHandle;
use crate::backend::setup;
use crate::backend::{
    BackendSession, StartupMcpServer, StartupMcpTransport, apply_session_settings_update,
    sanitize_session_settings_values, session_settings_schema_for_backend,
    validate_session_settings_values,
};
use crate::browse_stream;
use crate::config_mcp::ConfigMcpHandle;
use crate::debug_mcp::DebugMcpHandle;
use crate::error::{AppError, AppResult};
use crate::mobile_access::{
    MobileAccessCommand, MobileAccessHandle, MobileAccessInit, spawn_mobile_access_actor,
};
use crate::project_stream::{
    ProjectDiffRequestKey, ProjectStreamHandle, ProjectStreamSubscription, SearchSummary,
    build_dir_listing, commit, discard_file, is_not_git_repository_error, read_diff, read_file,
    search_project, spawn_project_subscription, stage_file, stage_hunk, unstage_file,
};
use crate::review::actor::{ReviewAiSpawnRequest, ReviewDeliveryOutcome, ReviewDeliveryRequest};
use crate::review::reviewer::{
    ReviewerToolBridge, build_reviewer_system_prompt, build_reviewer_user_prompt,
    reviewer_tool_policy,
};
use crate::review::{
    ReviewRegistry, ReviewRegistryHandle, build_create_request, review_create_selection,
    review_stream_path,
};
use crate::review_mcp::{REVIEW_FEEDBACK_MCP_SERVER_NAME, ReviewMcpHandle};
use crate::store::agent_teams::{AgentTeamValidationRefs, AgentTeamsStore};
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::{McpServerStore, RESERVED_MCP_SERVER_NAMES};
use crate::store::mobile_pairings::MobilePairingsStore;
use crate::store::project::{ProjectStore, ProjectStoreError};
use crate::store::review::ReviewStore;
use crate::store::session::{SessionRecord, SessionStore, session_record_is_resumable};
use crate::store::settings::HostSettingsStore;
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;
use crate::stream::{Stream, StreamClosed};
use crate::sub_agent::{
    HostSubAgentSpawnRequest, HostSubAgentSpawnRx, HostSubAgentSpawnTx, SubAgentEmitter,
    SubAgentHandle,
};
use crate::team_registry::{
    TeamDescribeData, TeamMemberActivation, TeamMessagePlan, TeamRegistryEvents,
    TeamRegistryHandle, TeamRegistrySnapshot, team_preset_validation_refs,
};
use crate::terminal_stream::{TerminalHandle, TerminalLaunchInfo, create_terminal};

struct HostSubscriber {
    stream: Stream,
    last_session_schemas: Option<Vec<SessionSchemaEntry>>,
}

#[derive(Clone, Debug, Default)]
pub struct HostRuntimeConfig {
    pub debug_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub agent_control_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub review_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub kiro_probe_program: Option<String>,
    pub mobile_pairing_ttl: Option<std::time::Duration>,
}

#[derive(Clone, Debug)]
struct TeamSpawnContext {
    team_id: TeamId,
    team_member_id: TeamMemberId,
}

#[derive(Debug)]
struct ReviewTargetAgentRequest {
    review_id: ReviewId,
    project_id: ProjectId,
    backend_kind: protocol::BackendKind,
    cost_hint: Option<protocol::SpawnCostHint>,
    custom_agent_id: Option<protocol::CustomAgentId>,
    name: Option<String>,
    instructions: Option<String>,
    payload: SendMessagePayload,
}

const COMPACTION_SUMMARY_PREVIEW_CHARS: usize = 512;

#[derive(Clone, Debug)]
pub(crate) struct TeamMemberMessageOutcome {
    pub member_id: TeamMemberId,
    pub agent_id: AgentId,
    pub queued: bool,
}

#[derive(Clone, Debug)]
struct TeamCompactTarget {
    member_id: TeamMemberId,
    agent_id: AgentId,
}

struct TeamAgentCompaction {
    target: TeamCompactTarget,
    compaction: AgentCompaction,
    rx: mpsc::UnboundedReceiver<protocol::Envelope>,
}

struct AgentCompaction {
    agent_id: AgentId,
    agent_handle: AgentHandle,
    start: AgentStartPayload,
    old_session_id: SessionId,
    access_mode: protocol::BackendAccessMode,
    old_record: SessionRecord,
    session_store: Arc<Mutex<SessionStore>>,
    team_registry: TeamRegistryHandle,
    stream: Stream,
    summary_rx: oneshot::Receiver<Result<CompactionSummary, String>>,
}

#[derive(Clone, Debug)]
enum KiroSessionSchemaState {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

pub(crate) struct HostState {
    pub registry: AgentRegistry,
    pub review_registry: ReviewRegistryHandle,
    pub team_registry: TeamRegistryHandle,
    pub project_store: Arc<Mutex<ProjectStore>>,
    pub settings_store: Arc<Mutex<HostSettingsStore>>,
    pub session_store: Arc<Mutex<SessionStore>>,
    pub custom_agent_store: Arc<Mutex<CustomAgentStore>>,
    pub mcp_server_store: Arc<Mutex<McpServerStore>>,
    pub steering_store: Arc<Mutex<SteeringStore>>,
    pub skill_store: Arc<Mutex<SkillStore>>,
    pub agent_sessions: HashMap<AgentId, SessionId>,
    pub sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub use_mock_backend: bool,
    pub debug_mcp: DebugMcpHandle,
    pub agent_control_mcp: AgentControlMcpHandle,
    pub config_mcp: ConfigMcpHandle,
    pub review_mcp: ReviewMcpHandle,
    pub mobile_access: MobileAccessHandle,
    kiro_session_schema: KiroSessionSchemaState,
    kiro_probe_program: Option<String>,
    host_streams: HashMap<StreamPath, HostSubscriber>,
    project_streams: HashMap<ProjectId, ProjectStreamSubscription>,
    terminal_streams: HashMap<(StreamPath, TerminalId), TerminalHandle>,
    browse_streams: HashMap<(StreamPath, StreamPath), Stream>,
    workbench_parent_locks: HashMap<ProjectId, Weak<Mutex<()>>>,
    /// Per-project "active search id". Each project-wide search stores its
    /// `search_id` here before spawning its walk; the walk polls this atomic
    /// and aborts as soon as the value no longer matches (a superseding search
    /// or an explicit cancel changed it).
    project_search_ids: HashMap<ProjectId, Arc<AtomicU64>>,
    /// Workbench projects currently being removed. Removal validates
    /// blockers against a snapshot and then runs slow git subprocesses
    /// outside the state lock; agent spawns and terminal creation check
    /// this set so they cannot race into a half-removed workbench.
    removing_projects: HashSet<ProjectId>,
}

impl Drop for HostState {
    fn drop(&mut self) {
        self.mobile_access.shutdown();
    }
}

#[derive(Clone)]
pub struct HostHandle {
    state: Arc<Mutex<HostState>>,
}

#[cfg(test)]
struct InstalledTeamMutationAfterRefsHook {
    inner: Arc<TeamMutationAfterRefsHook>,
}

#[cfg(test)]
struct TeamMutationAfterRefsHook {
    host_state_ptr: usize,
    operation: &'static str,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
type TeamMutationAfterRefsHookCell = std::sync::Mutex<Option<Arc<TeamMutationAfterRefsHook>>>;

#[cfg(test)]
impl InstalledTeamMutationAfterRefsHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    fn resume(&self) {
        self.inner.resume.notify_one();
    }
}

#[cfg(test)]
impl Drop for InstalledTeamMutationAfterRefsHook {
    fn drop(&mut self) {
        let mut hook = team_mutation_after_refs_hook_cell()
            .lock()
            .expect("team mutation hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_team_mutation_after_refs_test_hook(
    host: &HostHandle,
    operation: &'static str,
) -> InstalledTeamMutationAfterRefsHook {
    let inner = Arc::new(TeamMutationAfterRefsHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        operation,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let mut hook = team_mutation_after_refs_hook_cell()
        .lock()
        .expect("team mutation hook mutex poisoned");
    assert!(hook.is_none(), "team mutation test hook already installed");
    *hook = Some(Arc::clone(&inner));
    InstalledTeamMutationAfterRefsHook { inner }
}

#[cfg(test)]
async fn wait_for_team_mutation_after_refs_test_hook(host: &HostHandle, operation: &'static str) {
    let hook = {
        team_mutation_after_refs_hook_cell()
            .lock()
            .expect("team mutation hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize || hook.operation != operation {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn team_mutation_after_refs_hook_cell() -> &'static TeamMutationAfterRefsHookCell {
    static HOOK: std::sync::OnceLock<TeamMutationAfterRefsHookCell> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

pub(crate) struct HostSubAgentEmitter {
    spawn_tx: HostSubAgentSpawnTx,
    parent_agent_id: AgentId,
    workspace_roots: Vec<String>,
}

impl HostSubAgentEmitter {
    pub(crate) fn new(
        spawn_tx: HostSubAgentSpawnTx,
        parent_agent_id: AgentId,
        workspace_roots: Vec<String>,
    ) -> Self {
        Self {
            spawn_tx,
            parent_agent_id,
            workspace_roots,
        }
    }
}

struct HostStorePaths {
    session: PathBuf,
    project: PathBuf,
    agent_team: PathBuf,
    review: PathBuf,
    settings: PathBuf,
    custom_agent: PathBuf,
    mcp_server: PathBuf,
    steering: PathBuf,
    skills_index: PathBuf,
    skills_root_dir: PathBuf,
    mobile_pairings: PathBuf,
}

impl SubAgentEmitter for HostSubAgentEmitter {
    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<SessionId>,
    ) -> Pin<Box<dyn Future<Output = SubAgentHandle> + Send + '_>> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.spawn_tx
                .send(HostSubAgentSpawnRequest {
                    parent_agent_id: self.parent_agent_id.clone(),
                    workspace_roots: self.workspace_roots.clone(),
                    tool_use_id,
                    name,
                    description,
                    agent_type,
                    session_id_hint,
                    reply: reply_tx,
                })
                .unwrap_or_else(|_| {
                    panic!(
                        "host sub-agent spawn channel closed for parent {}",
                        self.parent_agent_id
                    )
                });
            reply_rx.await.unwrap_or_else(|_| {
                panic!(
                    "host sub-agent spawn reply dropped for parent {}",
                    self.parent_agent_id
                )
            })
        })
    }
}

impl HostHandle {
    pub(crate) async fn register_host_stream(
        &self,
        host_stream: Stream,
    ) -> Vec<(AgentHandle, Stream)> {
        let backend_setup = setup::collect_backend_setup().await;
        let mut state = self.state.lock().await;
        let host_path = host_stream.path().clone();

        let previous = state.host_streams.insert(
            host_path.clone(),
            HostSubscriber {
                stream: host_stream,
                last_session_schemas: None,
            },
        );
        assert!(
            previous.is_none(),
            "duplicate host stream registration for {}",
            host_path
        );

        // First-run: enable whichever backends are already installed so a fresh
        // install lands on a usable picker instead of an empty one. No-op once a
        // settings file exists.
        {
            let installed: Vec<protocol::BackendKind> = backend_setup
                .backends
                .iter()
                .filter(|info| info.status == protocol::BackendSetupStatus::Installed)
                .map(|info| info.backend_kind)
                .collect();
            match state
                .settings_store
                .lock()
                .await
                .seed_installed_backends_if_fresh(&installed)
            {
                Ok(true) => tracing::info!(
                    ?installed,
                    "seeded enabled backends from installed CLIs on first run"
                ),
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(%err, "failed to seed installed backends on first run")
                }
            }
        }

        // Pare-back: drop deprecated builtin custom agents (the old specialist
        // set) when the stored copy is an unedited published version and no
        // team member references it. User-edited copies and agents in use by
        // teams are preserved. Idempotent, so re-running per registration is
        // harmless.
        {
            let referenced: std::collections::HashSet<String> =
                match state.team_registry.snapshot().await {
                    Ok(snapshot) => snapshot
                        .members
                        .iter()
                        .filter_map(|member| member.custom_agent_id.as_ref())
                        .map(|id| id.0.clone())
                        .collect(),
                    Err(err) => {
                        tracing::warn!(%err, "skipping builtin agent pare-back: no team snapshot");
                        std::collections::HashSet::new()
                    }
                };
            let custom_agents = state.custom_agent_store.lock().await;
            for id in crate::store::custom_agents::deprecated_builtin_custom_agent_ids() {
                if referenced.contains(id) {
                    continue;
                }
                let agent_id = protocol::CustomAgentId(id.to_owned());
                let Some(record) = custom_agents.get(&agent_id) else {
                    continue;
                };
                if !crate::store::custom_agents::is_superseded_builtin(&record) {
                    continue;
                }
                match custom_agents.delete(&agent_id) {
                    Ok(_) => tracing::info!(%agent_id, "removed deprecated builtin custom agent"),
                    Err(err) => {
                        tracing::warn!(%agent_id, %err, "failed to remove deprecated builtin")
                    }
                }
            }
        }

        let settings = state
            .settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for registration: {err}"));
        let refresh_kiro_schema = settings
            .enabled_backends
            .contains(&protocol::BackendKind::Kiro);
        let schemas = session_schemas_for_enabled_backends(&state, &settings.enabled_backends);

        let mobile_access = {
            let Some(subscriber) = state.host_streams.get(&host_path) else {
                panic!(
                    "host stream {} disappeared during mobile access bootstrap registration",
                    host_path
                );
            };
            match state
                .mobile_access
                .register_bootstrap_subscriber(subscriber.stream.clone())
                .await
            {
                Ok(payload) => payload,
                Err(_) => {
                    state.host_streams.remove(&host_path);
                    return Vec::new();
                }
            }
        };

        let projects = state
            .project_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list projects for host registration: {err}"));
        let project_ids = projects
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();

        let mut sessions = state
            .session_store
            .lock()
            .await
            .summaries()
            .unwrap_or_else(|err| panic!("failed to list sessions for host registration: {err}"));
        normalize_antigravity_session_resumability(&mut sessions);

        let mcp_servers = state
            .mcp_server_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list MCP servers for host registration: {err}")
            });

        let skills = {
            let store = state.skill_store.lock().await;
            if let Err(err) = store.sync_from_disk() {
                tracing::warn!(
                    host_stream = %host_path,
                    error = %err,
                    "failed to sync skills for host registration; continuing with last known state"
                );
            }
            match store.list() {
                Ok(skills) => skills,
                Err(err) => {
                    tracing::warn!(
                        host_stream = %host_path,
                        error = %err,
                        "failed to list skills for host registration; continuing without skills"
                    );
                    Vec::new()
                }
            }
        };

        let steering = state
            .steering_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list steering for host registration: {err}"));

        let custom_agents = state
            .custom_agent_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list custom agents for host registration: {err}")
            });

        let team_snapshot = match state.team_registry.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::error!(
                    host_stream = %host_path,
                    error = %err,
                    "failed to snapshot teams for host registration"
                );
                state.mobile_access.unregister_subscriber(host_path.clone());
                state.host_streams.remove(&host_path);
                return Vec::new();
            }
        };

        let agent_ids = state.registry.agent_ids();
        let mut agents = Vec::new();
        let mut deferred_attachments = Vec::new();
        for agent_id in agent_ids {
            let agent_handle = state.registry.agent_handle(&agent_id).unwrap_or_else(|| {
                panic!(
                    "registry missing handle for listed agent {} during host stream registration",
                    agent_id
                )
            });
            let start = agent_handle.snapshot();
            let Some(subscriber) = state.host_streams.get(&host_path) else {
                panic!(
                    "host stream {} disappeared during registration bootstrap build",
                    host_path
                );
            };
            let instance_stream = new_instance_stream(&start.agent_id);
            let new_agent = NewAgentPayload {
                agent_id: start.agent_id.clone(),
                name: start.name.clone(),
                origin: start.origin,
                backend_kind: start.backend_kind,
                workspace_roots: start.workspace_roots.clone(),
                custom_agent_id: start.custom_agent_id.clone(),
                team_id: start.team_id.clone(),
                team_member_id: start.team_member_id.clone(),
                project_id: start.project_id.clone(),
                parent_agent_id: start.parent_agent_id.clone(),
                session_id: start.session_id.clone(),
                created_at_ms: start.created_at_ms,
                instance_stream: instance_stream.clone(),
            };
            let agent_stream = subscriber.stream.with_path(instance_stream);
            agents.push(new_agent);
            deferred_attachments.push((agent_handle, agent_stream));
        }

        let bootstrap = HostBootstrapPayload {
            settings,
            mobile_access,
            backend_setup,
            session_schemas: schemas,
            sessions,
            projects,
            mcp_servers,
            skills,
            steering,
            custom_agents,
            team_preset_catalog: team_snapshot.catalog,
            team_drafts: team_snapshot.drafts,
            teams: team_snapshot.teams,
            team_members: team_snapshot.members,
            team_member_bindings: team_snapshot.bindings,
            agents,
        };

        let payload = serde_json::to_value(&bootstrap)
            .expect("failed to serialize HostBootstrap payload for host stream registration");
        let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
            panic!(
                "host stream {} disappeared before HostBootstrap emission",
                host_path
            );
        };
        if subscriber
            .stream
            .send_value(FrameKind::HostBootstrap, payload)
            .is_err()
        {
            state.mobile_access.unregister_subscriber(host_path.clone());
            state.host_streams.remove(&host_path);
            return Vec::new();
        }
        subscriber.last_session_schemas = Some(bootstrap.session_schemas.clone());
        state
            .mobile_access
            .activate_bootstrap_subscriber(host_path.clone());

        for project_id in project_ids {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to project stream during registration"
                );
            }
        }

        drop(state);
        if refresh_kiro_schema {
            self.schedule_session_schema_refresh();
        }
        deferred_attachments
    }

    pub(crate) async fn unregister_host_stream(&self, path: &StreamPath) {
        let (project_handles, terminals, review_registry) = {
            let mut state = self.state.lock().await;
            state.host_streams.remove(path);
            state.mobile_access.unregister_subscriber(path.clone());
            let review_registry = state.review_registry.clone();
            let project_handles = state
                .project_streams
                .values()
                .map(|subscription| subscription.handle.clone())
                .collect::<Vec<_>>();

            let browse_keys = state
                .browse_streams
                .keys()
                .filter(|(host_stream, _)| host_stream == path)
                .cloned()
                .collect::<Vec<_>>();
            for key in browse_keys {
                state.browse_streams.remove(&key);
            }

            let terminal_keys = state
                .terminal_streams
                .keys()
                .filter(|(host_stream, _)| host_stream == path)
                .cloned()
                .collect::<Vec<_>>();
            let mut terminals = Vec::with_capacity(terminal_keys.len());
            for key in terminal_keys {
                let Some(terminal) = state.terminal_streams.remove(&key) else {
                    continue;
                };
                terminals.push(terminal);
            }
            (project_handles, terminals, review_registry)
        };

        for handle in project_handles {
            handle.remove_subscriber(path.clone()).await;
        }

        review_registry.unsubscribe_all(path.clone()).await;

        for terminal in terminals {
            terminal.close().await;
        }
    }

    async fn workbench_parent_lock(&self, parent_project_id: &ProjectId) -> Arc<Mutex<()>> {
        let mut state = self.state.lock().await;
        state
            .workbench_parent_locks
            .retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = state
            .workbench_parent_locks
            .get(parent_project_id)
            .and_then(Weak::upgrade)
        {
            return lock;
        }

        let lock = Arc::new(Mutex::new(()));
        state
            .workbench_parent_locks
            .insert(parent_project_id.clone(), Arc::downgrade(&lock));
        lock
    }

    pub(crate) async fn spawn_agent(&self, payload: SpawnAgentPayload) -> AgentId {
        self.spawn_agent_with_origin(payload, AgentOrigin::User)
            .await
    }

    pub(crate) async fn compact_agent_in_background(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let Some(compaction) = self
            .begin_agent_compaction(agent_id, payload, stream)
            .await?
        else {
            return Ok(());
        };
        let host = self.clone();
        tokio::spawn(async move {
            host.finish_agent_compaction(compaction).await;
        });
        Ok(())
    }

    pub(crate) async fn compact_team(
        &self,
        payload: TeamCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let targets = self.team_compact_targets(&payload.team_id).await?;
        let member_ids = targets
            .iter()
            .map(|target| target.member_id.clone())
            .collect::<Vec<_>>();
        let agent_ids = targets
            .iter()
            .map(|target| target.agent_id.clone())
            .collect::<Vec<_>>();
        let mut compactions = Vec::with_capacity(targets.len());
        let mut results = Vec::new();
        for target in targets {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let target_agent_id = target.agent_id.clone();
            let agent_stream = Stream::new(
                StreamPath(format!("/agent/{}/team-compact", target_agent_id)),
                tx,
            );
            let agent_payload = AgentCompactPayload {
                summary_prompt: payload.summary_prompt.clone(),
                max_summary_bytes: payload.max_summary_bytes,
            };
            match self
                .begin_agent_compaction(target_agent_id.clone(), agent_payload, agent_stream)
                .await?
            {
                Some(compaction) => {
                    compactions.push(TeamAgentCompaction {
                        target,
                        compaction,
                        rx,
                    });
                }
                None => {
                    results.push(drain_final_agent_compact_notify(&target_agent_id, &mut rx));
                }
            }
        }

        send_team_compact_notify(
            &stream,
            TeamCompactNotifyPayload {
                status: TeamCompactStatus::Started,
                team_id: payload.team_id.clone(),
                member_ids: member_ids.clone(),
                agent_ids: agent_ids.clone(),
                results: Vec::new(),
                message: None,
            },
        );

        let host = self.clone();
        tokio::spawn(async move {
            host.finish_team_compactions(
                payload,
                member_ids,
                agent_ids,
                compactions,
                results,
                stream,
            )
            .await;
        });
        Ok(())
    }

    async fn team_compact_targets(&self, team_id: &TeamId) -> AppResult<Vec<TeamCompactTarget>> {
        const OPERATION: &str = "team_compact";
        let registry = { self.state.lock().await.team_registry.clone() };
        let snapshot = registry
            .snapshot()
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        if !snapshot.teams.iter().any(|team| team.id == *team_id) {
            return Err(AppError::not_found(
                OPERATION,
                format!("team {team_id} does not exist"),
            ));
        }

        let members = snapshot
            .members
            .into_iter()
            .filter(|member| member.team_id == *team_id && member.state == TeamMemberState::Active)
            .collect::<Vec<_>>();
        let bindings = snapshot
            .bindings
            .into_iter()
            .map(|binding| (binding.member_id.clone(), binding))
            .collect::<HashMap<_, _>>();
        let mut targets = Vec::new();
        for member in members {
            let Some(binding) = bindings.get(&member.id) else {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!("team member {} has no binding", member.id),
                    anyhow!("team member {} has no binding", member.id),
                ));
            };
            if binding.status != AgentControlStatus::Idle {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "team member {} is not idle ({:?})",
                        member.id, binding.status
                    ),
                ));
            }
            let Some(agent_id) = binding.current_agent_id.clone() else {
                continue;
            };
            let status = self.agent_status_snapshot(&agent_id).await.ok_or_else(|| {
                AppError::conflict(
                    OPERATION,
                    format!(
                        "team member {} is bound to missing agent {agent_id}",
                        member.id
                    ),
                )
            })?;
            if status.terminated {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("team member {} agent {agent_id} is terminated", member.id),
                ));
            }
            if status.is_active() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("team member {} agent {agent_id} is not idle", member.id),
                ));
            }
            targets.push(TeamCompactTarget {
                member_id: member.id,
                agent_id,
            });
        }

        if targets.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                format!("team {team_id} has no live idle agents to compact"),
            ));
        }
        Ok(targets)
    }

    async fn finish_team_compactions(
        &self,
        payload: TeamCompactPayload,
        member_ids: Vec<TeamMemberId>,
        agent_ids: Vec<AgentId>,
        compactions: Vec<TeamAgentCompaction>,
        mut results: Vec<AgentCompactNotifyPayload>,
        stream: Stream,
    ) {
        let mut handles = Vec::with_capacity(compactions.len());
        for compaction in compactions {
            let host = self.clone();
            let target = compaction.target.clone();
            handles.push((
                target,
                tokio::spawn(async move { host.finish_team_compaction_target(compaction).await }),
            ));
        }

        for (target, handle) in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(error) => results.push(AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: target.agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!("team compaction task failed: {error}")),
                }),
            }
        }

        let failures = results
            .iter()
            .filter(|result| result.status != AgentCompactStatus::Completed)
            .count();
        let (status, message) = if failures == 0 {
            (TeamCompactStatus::Completed, None)
        } else {
            (
                TeamCompactStatus::Failed,
                Some(format!(
                    "{failures} of {} team agents failed to compact",
                    results.len()
                )),
            )
        };
        send_team_compact_notify(
            &stream,
            TeamCompactNotifyPayload {
                status,
                team_id: payload.team_id,
                member_ids,
                agent_ids,
                results,
                message,
            },
        );
    }

    async fn finish_team_compaction_target(
        &self,
        team_compaction: TeamAgentCompaction,
    ) -> AgentCompactNotifyPayload {
        let old_agent_id = team_compaction.target.agent_id.clone();
        self.finish_agent_compaction(team_compaction.compaction)
            .await;
        let mut rx = team_compaction.rx;
        drain_final_agent_compact_notify(&old_agent_id, &mut rx)
    }

    #[cfg(test)]
    pub(crate) async fn compact_agent(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let Some(compaction) = self
            .begin_agent_compaction(agent_id, payload, stream)
            .await?
        else {
            return Ok(());
        };
        self.finish_agent_compaction(compaction).await;
        Ok(())
    }

    async fn begin_agent_compaction(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<Option<AgentCompaction>> {
        let summary_prompt = payload
            .summary_prompt
            .unwrap_or_else(default_compaction_summary_prompt);
        let max_summary_bytes = payload
            .max_summary_bytes
            .map(|bytes| bytes as usize)
            .unwrap_or(DEFAULT_COMPACTION_SUMMARY_MAX_BYTES)
            .clamp(1, MAX_COMPACTION_SUMMARY_BYTES);

        let Some((agent_handle, start, old_session_id, access_mode, session_store, team_registry)) =
            ({
                let state = self.state.lock().await;
                state.registry.agent_handle(&agent_id).map(|agent_handle| {
                    let start = agent_handle.snapshot();
                    (
                        agent_handle,
                        start,
                        state.agent_sessions.get(&agent_id).cloned(),
                        state.registry.agent_access_mode(&agent_id),
                        Arc::clone(&state.session_store),
                        state.team_registry.clone(),
                    )
                })
            })
        else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent is not running".to_owned()),
                },
            );
            return Ok(None);
        };

        let Some(old_session_id) = old_session_id else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent has no session to compact".to_owned()),
                },
            );
            return Ok(None);
        };
        let Some(access_mode) = access_mode else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent access mode is unavailable".to_owned()),
                },
            );
            return Ok(None);
        };
        if start.origin == AgentOrigin::BackendNative {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("backend-native agents cannot be compacted".to_owned()),
                },
            );
            return Ok(None);
        }
        let old_record = match session_store.lock().await.get(&old_session_id) {
            Some(record) => record,
            None => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent session metadata is missing".to_owned()),
                    },
                );
                return Ok(None);
            }
        };
        if old_record.compacted_to_session_id.is_some() {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent session is already compacted".to_owned()),
                },
            );
            return Ok(None);
        }

        send_agent_compact_notify(
            &stream,
            AgentCompactNotifyPayload {
                status: AgentCompactStatus::Started,
                old_agent_id: agent_id.clone(),
                old_session_id: Some(old_session_id.clone()),
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );

        let summary_rx = match agent_handle.begin_compact(summary_prompt, max_summary_bytes) {
            CompactionStart::Started(summary_rx) => summary_rx,
            CompactionStart::Rejected(error) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some(error),
                    },
                );
                return Ok(None);
            }
            CompactionStart::Closed => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent stopped before compaction completed".to_owned()),
                    },
                );
                return Ok(None);
            }
        };
        Ok(Some(AgentCompaction {
            agent_id,
            agent_handle,
            start,
            old_session_id,
            access_mode,
            old_record,
            session_store,
            team_registry,
            stream,
            summary_rx,
        }))
    }

    async fn finish_agent_compaction(&self, compaction: AgentCompaction) {
        let AgentCompaction {
            agent_id,
            agent_handle,
            start,
            old_session_id,
            access_mode,
            old_record,
            session_store,
            team_registry,
            stream,
            summary_rx,
        } = compaction;
        let summary = match summary_rx.await {
            Ok(Ok(summary)) => summary,
            Ok(Err(error)) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some(error),
                    },
                );
                return;
            }
            Err(_) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent stopped before compaction completed".to_owned()),
                    },
                );
                return;
            }
        };
        if summary.session_id != old_session_id {
            let _ = agent_handle.release_compaction().await;
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!(
                        "compaction summary came from unexpected session {}",
                        summary.session_id
                    )),
                },
            );
            return;
        }

        let summary_preview = compaction_summary_preview(&summary.summary);
        let replacement_prompt = build_compaction_replacement_prompt(&summary.summary);
        let team_context = match (
            start.origin,
            start.team_id.clone(),
            start.team_member_id.clone(),
        ) {
            (AgentOrigin::TeamMember, Some(team_id), Some(team_member_id)) => {
                Some(TeamSpawnContext {
                    team_id,
                    team_member_id,
                })
            }
            (AgentOrigin::TeamMember, _, _) => {
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some("team agent metadata is incomplete".to_owned()),
                    },
                );
                return;
            }
            (_, None, None) => None,
            _ => {
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some("team agent metadata is incomplete".to_owned()),
                    },
                );
                return;
            }
        };
        let replacement_payload = SpawnAgentPayload {
            name: Some(start.name.clone()),
            custom_agent_id: start.custom_agent_id.clone(),
            parent_agent_id: start.parent_agent_id.clone(),
            project_id: start.project_id.clone(),
            params: SpawnAgentParams::New {
                workspace_roots: start.workspace_roots.clone(),
                prompt: replacement_prompt,
                images: None,
                backend_kind: start.backend_kind,
                cost_hint: None,
                access_mode,
                session_settings: old_record.session_settings.clone(),
            },
        };
        let new_agent_id = self
            .spawn_agent_with_origin_config_and_team(
                replacement_payload,
                start.origin,
                None,
                team_context.clone(),
            )
            .await;
        let new_session_id = match self.wait_for_agent_session_id_result(&new_agent_id).await {
            Ok(session_id) => session_id,
            Err(error) => {
                self.close_agent(&new_agent_id).await;
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: Some(new_agent_id),
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some(format!("replacement agent failed to start: {error}")),
                    },
                );
                return;
            }
        };

        if let Some(context) = team_context.as_ref() {
            let refs_result = {
                let state = self.state.lock().await;
                agent_team_validation_refs(&state, "agent_compact").await
            };
            let refs = match refs_result {
                Ok(refs) => refs,
                Err(error) => {
                    self.close_agent(&new_agent_id).await;
                    let _ = agent_handle.release_compaction().await;
                    send_agent_compact_notify(
                        &stream,
                        AgentCompactNotifyPayload {
                            status: AgentCompactStatus::Failed,
                            old_agent_id: agent_id,
                            old_session_id: Some(old_session_id),
                            new_agent_id: Some(new_agent_id),
                            new_session_id: Some(new_session_id),
                            summary_preview: Some(summary_preview),
                            message: Some(format!("team validation failed: {error}")),
                        },
                    );
                    return;
                }
            };
            match team_registry
                .rotate_member_agent(
                    context.team_member_id.clone(),
                    agent_id.clone(),
                    new_agent_id.clone(),
                    old_session_id.clone(),
                    new_session_id.clone(),
                    refs,
                )
                .await
            {
                Ok(events) => {
                    let mut state = self.state.lock().await;
                    fan_out_team_registry_events(&mut state, events).await;
                }
                Err(error) => {
                    self.close_agent(&new_agent_id).await;
                    let _ = agent_handle.release_compaction().await;
                    send_agent_compact_notify(
                        &stream,
                        AgentCompactNotifyPayload {
                            status: AgentCompactStatus::Failed,
                            old_agent_id: agent_id,
                            old_session_id: Some(old_session_id),
                            new_agent_id: Some(new_agent_id),
                            new_session_id: Some(new_session_id),
                            summary_preview: Some(summary_preview),
                            message: Some(format!("team binding rotation failed: {error}")),
                        },
                    );
                    return;
                }
            }
        }

        if let Err(error) = session_store.lock().await.mark_compacted(
            &old_session_id,
            &new_session_id,
            summary_preview.clone(),
        ) {
            if let Some(context) = team_context.as_ref() {
                let rollback_refs_result = {
                    let state = self.state.lock().await;
                    agent_team_validation_refs(&state, "agent_compact_rollback").await
                };
                match rollback_refs_result {
                    Ok(rollback_refs) => {
                        match team_registry
                            .rotate_member_agent(
                                context.team_member_id.clone(),
                                new_agent_id.clone(),
                                agent_id.clone(),
                                new_session_id.clone(),
                                old_session_id.clone(),
                                rollback_refs,
                            )
                            .await
                        {
                            Ok(events) => {
                                let mut state = self.state.lock().await;
                                fan_out_team_registry_events(&mut state, events).await;
                            }
                            Err(rollback_error) => {
                                tracing::error!(
                                    member_id = %context.team_member_id,
                                    error = %rollback_error,
                                    "failed to roll back team binding after session compaction metadata failure"
                                );
                            }
                        }
                    }
                    Err(rollback_error) => {
                        tracing::error!(
                            member_id = %context.team_member_id,
                            error = %rollback_error,
                            "failed to validate team binding rollback after session compaction metadata failure"
                        );
                    }
                }
            }
            self.close_agent(&new_agent_id).await;
            let _ = agent_handle.release_compaction().await;
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: Some(new_agent_id),
                    new_session_id: Some(new_session_id),
                    summary_preview: Some(summary_preview),
                    message: Some(format!("session compaction metadata failed: {error}")),
                },
            );
            return;
        }

        self.fan_out_session_lists().await;
        send_agent_compact_notify(
            &stream,
            AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: agent_id.clone(),
                old_session_id: Some(old_session_id),
                new_agent_id: Some(new_agent_id),
                new_session_id: Some(new_session_id),
                summary_preview: Some(summary_preview),
                message: None,
            },
        );
        if !self.close_agent(&agent_id).await {
            tracing::warn!(
                agent_id = %agent_id,
                "old agent was already closed after successful compaction"
            );
        }
    }

    async fn spawn_agent_with_origin(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
    ) -> AgentId {
        self.spawn_agent_with_origin_and_config(payload, origin, None)
            .await
    }

    async fn spawn_agent_with_origin_and_config(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
        resolved_spawn_config_override: Option<ResolvedSpawnConfig>,
    ) -> AgentId {
        self.spawn_agent_with_origin_config_and_team(
            payload,
            origin,
            resolved_spawn_config_override,
            None,
        )
        .await
    }

    async fn spawn_agent_with_origin_config_and_team(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
        resolved_spawn_config_override: Option<ResolvedSpawnConfig>,
        team_context: Option<TeamSpawnContext>,
    ) -> AgentId {
        tracing::info!(
            parent_agent_id = ?payload.parent_agent_id,
            project_id = ?payload.project_id,
            requested_name = ?payload.name,
            "host spawn_agent requested"
        );
        let (
            session_store,
            project_store,
            settings_store,
            custom_agent_store,
            mcp_server_store,
            steering_store,
            skill_store,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp,
            config_mcp,
            removing_projects,
            parent_session_id,
        ) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_store),
                Arc::clone(&state.project_store),
                Arc::clone(&state.settings_store),
                Arc::clone(&state.custom_agent_store),
                Arc::clone(&state.mcp_server_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.skill_store),
                state.use_mock_backend,
                state.debug_mcp.clone(),
                state.agent_control_mcp.clone(),
                state.config_mcp.clone(),
                state.removing_projects.clone(),
                payload.parent_agent_id.as_ref().map(|agent_id| {
                    if let Some(session_id) = state.agent_sessions.get(agent_id).cloned() {
                        Ok(session_id)
                    } else if let Some(handle) = state.registry.agent_handle(agent_id) {
                        handle.snapshot().session_id.ok_or_else(|| {
                            AgentStartupFailure::internal(format!(
                                "fork parent_agent_id {} has no known session_id",
                                agent_id
                            ))
                        })
                    } else {
                        Err(AgentStartupFailure::internal(format!(
                            "fork parent_agent_id {} is not running",
                            agent_id
                        )))
                    }
                }),
            )
        };
        let (parent_session_id, parent_session_lookup_failure) = match parent_session_id {
            Some(Ok(session_id)) => (Some(session_id), None),
            Some(Err(err)) => (None, Some(err)),
            None => (None, None),
        };
        let mut deferred_generated_name: Option<GenerateAgentNameRequest> = None;
        let host_settings = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for spawn: {err}"));

        let request = match payload.params {
            SpawnAgentParams::New {
                workspace_roots,
                prompt,
                images,
                backend_kind,
                cost_hint,
                access_mode,
                session_settings,
            } => {
                let (project_id, missing_project_failure) = match payload.project_id.clone() {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            (
                                None,
                                Some(format!(
                                    "cannot spawn agent in workbench {} because it is being removed",
                                    project_id
                                )),
                            )
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            (
                                None,
                                Some(format!(
                                    "cannot spawn agent in missing project {}",
                                    project_id
                                )),
                            )
                        }
                    }
                    None => (None, None),
                };
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    payload.custom_agent_id.as_ref(),
                );
                let requested_custom_agent_id = payload.custom_agent_id.clone();
                let (
                    effective_custom_agent_id,
                    mut resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(err) = missing_project_failure {
                    (
                        requested_custom_agent_id,
                        ResolvedSpawnConfig::default(),
                        None,
                        Some(AgentStartupFailure::backend_failed(err)),
                    )
                } else if let Some(resolved) = resolved_spawn_config_override.clone() {
                    (None, resolved, None, None)
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: requested_custom_agent_id.as_ref(),
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (requested_custom_agent_id, resolved, None, None),
                        Err(err) => (
                            requested_custom_agent_id,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                if resolved_spawn_config_override.is_none() {
                    resolved_spawn_config.access_mode = access_mode;
                }
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let session_settings_schema = {
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, backend_kind)
                };
                let session_settings_schema = if backend_kind == protocol::BackendKind::Kiro
                    && session_settings_schema.is_none()
                {
                    self.refresh_session_schemas().await;
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, backend_kind)
                } else {
                    session_settings_schema
                };
                if let Some(resolved_session_settings) = session_settings.as_ref()
                    && !resolved_session_settings.0.is_empty()
                {
                    let schema = session_settings_schema.as_ref().unwrap_or_else(|| {
                        panic!(
                            "session settings schema unavailable for backend {:?}",
                            backend_kind
                        )
                    });
                    validate_session_settings_values(schema, resolved_session_settings)
                        .unwrap_or_else(|err| {
                            panic!(
                                "invalid session settings for backend {:?}: {err}",
                                backend_kind
                            )
                        });
                }
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let provisional = derive_agent_name(&prompt);
                        if startup_failure.is_none() {
                            deferred_generated_name = Some(GenerateAgentNameRequest {
                                backend_kind,
                                workspace_roots: workspace_roots.clone(),
                                prompt: prompt.clone(),
                                startup_mcp_servers: startup_mcp_servers.clone(),
                                use_mock_backend,
                            });
                        }
                        (
                            provisional.clone(),
                            Some(InitialAgentAlias {
                                name: provisional,
                                persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                            }),
                        )
                    }
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                    team_member_id: team_context
                        .as_ref()
                        .map(|context| context.team_member_id.clone()),
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id,
                    backend_kind,
                    workspace_roots,
                    initial_input: Some(protocol::SendMessagePayload {
                        message: prompt,
                        images,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint,
                    session_settings,
                    session_settings_schema,
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: None,
                    fork_from_session_id: None,
                    startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Resume { session_id, prompt } => {
                let record = session_store.lock().await.get(&session_id);
                let Some(record) = record else {
                    let resolved_name = payload
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Session {}", session_id));
                    let initial_alias = payload.name.clone().map(|name| InitialAgentAlias {
                        name,
                        persistence: InitialAgentAliasPersistence::User,
                    });
                    return self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin,
                            custom_agent_id: payload.custom_agent_id.clone(),
                            team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                            team_member_id: team_context
                                .as_ref()
                                .map(|context| context.team_member_id.clone()),
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id,
                            project_id: payload.project_id.clone(),
                            backend_kind: host_settings
                                .default_backend
                                .or_else(|| host_settings.enabled_backends.first().copied())
                                .unwrap_or(protocol::BackendKind::Claude),
                            workspace_roots: Vec::new(),
                            initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                                message: prompt,
                                images: None,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config: ResolvedSpawnConfig::default(),
                            resume_session_id: Some(session_id.clone()),
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::unsupported(format!(
                                "cannot resume missing session {}",
                                session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await;
                };
                if !session_record_is_resumable(&record) {
                    let resolved_name = payload
                        .name
                        .clone()
                        .or_else(|| record.user_alias.clone())
                        .or_else(|| record.alias.clone())
                        .unwrap_or_else(|| format!("Session {}", session_id));
                    let initial_alias = payload.name.clone().map(|name| InitialAgentAlias {
                        name,
                        persistence: InitialAgentAliasPersistence::User,
                    });
                    return self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin,
                            custom_agent_id: record.custom_agent_id.clone(),
                            team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                            team_member_id: team_context
                                .as_ref()
                                .map(|context| context.team_member_id.clone()),
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id,
                            project_id: record.project_id.clone(),
                            backend_kind: record.backend_kind,
                            workspace_roots: record.workspace_roots.clone(),
                            initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                                message: prompt,
                                images: None,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config: ResolvedSpawnConfig::default(),
                            resume_session_id: Some(session_id.clone()),
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::unsupported(format!(
                                "cannot resume non-resumable session {}",
                                session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await;
                }
                if let Some(requested_custom_agent_id) = payload.custom_agent_id.as_ref() {
                    assert_eq!(
                        record.custom_agent_id.as_ref(),
                        Some(requested_custom_agent_id),
                        "resume custom_agent_id {:?} must match stored session custom_agent_id {:?}",
                        requested_custom_agent_id,
                        record.custom_agent_id
                    );
                }
                let requested_project_id = payload.project_id.or(record.project_id.clone());
                let (project_id, missing_project_warning) = match requested_project_id {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            let warning = format!(
                                "workbench {} is being removed; resuming without a project",
                                project_id
                            );
                            (None, Some(warning))
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            let warning = format!(
                                "project {} was deleted; resuming without a project",
                                project_id
                            );
                            (None, Some(warning))
                        }
                    }
                    None => (None, None),
                };
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &record.workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    record.custom_agent_id.as_ref(),
                );
                let (
                    effective_custom_agent_id,
                    resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(stored_custom_agent_id) = record.custom_agent_id.as_ref() {
                    if custom_agent_store
                        .lock()
                        .await
                        .get(stored_custom_agent_id)
                        .is_none()
                    {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        (
                            None,
                            resolve_spawn_config(ResolveSpawnConfigRequest {
                                backend_kind: record.backend_kind,
                                project_id: project_id.as_ref(),
                                custom_agent_id: None,
                                built_in_mcp_servers: &startup_mcp_servers,
                                custom_agent_store: &custom_agents,
                                mcp_server_store: &mcp_servers,
                                steering_store: &steering,
                                skill_store: &skills,
                            })
                            .unwrap_or_else(|err| {
                                panic!(
                                    "failed to resolve resume customization after deleted custom agent {}: {err}",
                                    stored_custom_agent_id
                                )
                            }),
                            Some(format!(
                                "custom agent {} was deleted; resuming without custom agent configuration",
                                stored_custom_agent_id
                            )),
                            None,
                        )
                    } else {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        match resolve_spawn_config(ResolveSpawnConfigRequest {
                            backend_kind: record.backend_kind,
                            project_id: project_id.as_ref(),
                            custom_agent_id: Some(stored_custom_agent_id),
                            built_in_mcp_servers: &startup_mcp_servers,
                            custom_agent_store: &custom_agents,
                            mcp_server_store: &mcp_servers,
                            steering_store: &steering,
                            skill_store: &skills,
                        }) {
                            Ok(resolved) => {
                                (Some(stored_custom_agent_id.clone()), resolved, None, None)
                            }
                            Err(err) => (
                                Some(stored_custom_agent_id.clone()),
                                ResolvedSpawnConfig::default(),
                                None,
                                Some(AgentStartupFailure::internal(err)),
                            ),
                        }
                    }
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind: record.backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: None,
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (None, resolved, None, None),
                        Err(err) => (
                            None,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let session_settings_schema = {
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, record.backend_kind)
                };
                let session_settings_schema = if record.backend_kind == protocol::BackendKind::Kiro
                    && session_settings_schema.is_none()
                {
                    self.refresh_session_schemas().await;
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, record.backend_kind)
                } else {
                    session_settings_schema
                };
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let effective = record
                            .user_alias
                            .clone()
                            .or(record.alias.clone())
                            .unwrap_or_else(|| {
                                panic!(
                                    "cannot resume session {} without a stored effective name",
                                    session_id
                                )
                            });
                        (effective, None)
                    }
                };
                let sanitized_settings = record.session_settings.clone().map(|stored_settings| {
                    if stored_settings.0.is_empty() {
                        return stored_settings;
                    }
                    let schema = session_settings_schema.as_ref().unwrap_or_else(|| {
                        panic!(
                            "session settings schema unavailable for backend {:?}",
                            record.backend_kind
                        )
                    });
                    sanitize_session_settings_values(schema, &stored_settings)
                });
                let combined_startup_warning = match (startup_warning, missing_project_warning) {
                    (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                    team_member_id: team_context
                        .as_ref()
                        .map(|context| context.team_member_id.clone()),
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id,
                    backend_kind: record.backend_kind,
                    workspace_roots: record.workspace_roots,
                    initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                        message: prompt,
                        images: None,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint: None,
                    session_settings: sanitized_settings,
                    session_settings_schema,
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: Some(session_id),
                    fork_from_session_id: None,
                    startup_warning: combined_startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Fork {
                from_session_id,
                prompt,
                images,
                access_mode,
            } => {
                assert!(
                    payload.parent_agent_id.is_some(),
                    "fork spawn requires parent_agent_id"
                );
                let record = session_store.lock().await.get(&from_session_id);
                let Some(record) = record else {
                    let resolved_name = payload.name.clone().unwrap_or_else(|| {
                        let prompt_name = derive_agent_name(&prompt);
                        format!("BTW: {prompt_name}")
                    });
                    let initial_alias = Some(InitialAgentAlias {
                        name: resolved_name.clone(),
                        persistence: if payload.name.is_some() {
                            InitialAgentAliasPersistence::User
                        } else {
                            InitialAgentAliasPersistence::GeneratedIfNoUserAlias
                        },
                    });
                    let resolved_spawn_config = ResolvedSpawnConfig {
                        access_mode: access_mode.unwrap_or(protocol::BackendAccessMode::ReadOnly),
                        ..Default::default()
                    };
                    return self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin: AgentOrigin::SideQuestion,
                            custom_agent_id: payload.custom_agent_id,
                            team_id: None,
                            team_member_id: None,
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id: Some(from_session_id.clone()),
                            project_id: payload.project_id,
                            backend_kind: protocol::BackendKind::Claude,
                            workspace_roots: Vec::new(),
                            initial_input: Some(protocol::SendMessagePayload {
                                message: prompt,
                                images,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config,
                            resume_session_id: None,
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::internal(format!(
                                "cannot fork missing session {}",
                                from_session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await;
                };
                if let Some(requested_custom_agent_id) = payload.custom_agent_id.as_ref() {
                    assert_eq!(
                        record.custom_agent_id.as_ref(),
                        Some(requested_custom_agent_id),
                        "fork custom_agent_id {:?} must match stored session custom_agent_id {:?}",
                        requested_custom_agent_id,
                        record.custom_agent_id
                    );
                }

                let requested_project_id = record.project_id.clone();
                let (project_id, missing_project_warning) = match requested_project_id {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            let warning = format!(
                                "workbench {} is being removed; forking without a project",
                                project_id
                            );
                            (None, Some(warning))
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            let warning = format!(
                                "project {} was deleted; forking without a project",
                                project_id
                            );
                            (None, Some(warning))
                        }
                    }
                    None => (None, None),
                };
                let workspace_roots = record.workspace_roots.clone();
                let backend_kind = record.backend_kind;
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    record.custom_agent_id.as_ref(),
                );
                let (
                    effective_custom_agent_id,
                    mut resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(stored_custom_agent_id) = record.custom_agent_id.as_ref() {
                    if custom_agent_store
                        .lock()
                        .await
                        .get(stored_custom_agent_id)
                        .is_none()
                    {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        (
                            None,
                            resolve_spawn_config(ResolveSpawnConfigRequest {
                                backend_kind,
                                project_id: project_id.as_ref(),
                                custom_agent_id: None,
                                built_in_mcp_servers: &startup_mcp_servers,
                                custom_agent_store: &custom_agents,
                                mcp_server_store: &mcp_servers,
                                steering_store: &steering,
                                skill_store: &skills,
                            })
                            .unwrap_or_else(|err| {
                                panic!(
                                    "failed to resolve fork customization after deleted custom agent {}: {err}",
                                    stored_custom_agent_id
                                )
                            }),
                            Some(format!(
                                "custom agent {} was deleted; forking without custom agent configuration",
                                stored_custom_agent_id
                            )),
                            None,
                        )
                    } else {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        match resolve_spawn_config(ResolveSpawnConfigRequest {
                            backend_kind,
                            project_id: project_id.as_ref(),
                            custom_agent_id: Some(stored_custom_agent_id),
                            built_in_mcp_servers: &startup_mcp_servers,
                            custom_agent_store: &custom_agents,
                            mcp_server_store: &mcp_servers,
                            steering_store: &steering,
                            skill_store: &skills,
                        }) {
                            Ok(resolved) => {
                                (Some(stored_custom_agent_id.clone()), resolved, None, None)
                            }
                            Err(err) => (
                                Some(stored_custom_agent_id.clone()),
                                ResolvedSpawnConfig::default(),
                                None,
                                Some(AgentStartupFailure::internal(err)),
                            ),
                        }
                    }
                } else if let Some(resolved) = resolved_spawn_config_override.clone() {
                    (None, resolved, None, None)
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: None,
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (None, resolved, None, None),
                        Err(err) => (
                            None,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                resolved_spawn_config.access_mode =
                    access_mode.unwrap_or(protocol::BackendAccessMode::ReadOnly);
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let session_settings_schema = {
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, backend_kind)
                };
                let session_settings_schema = if backend_kind == protocol::BackendKind::Kiro
                    && session_settings_schema.is_none()
                {
                    self.refresh_session_schemas().await;
                    let state = self.state.lock().await;
                    session_schema_for_backend(&state, backend_kind)
                } else {
                    session_settings_schema
                };
                let sanitized_settings = record.session_settings.clone().map(|stored_settings| {
                    if stored_settings.0.is_empty() {
                        return stored_settings;
                    }
                    let schema = session_settings_schema.as_ref().unwrap_or_else(|| {
                        panic!(
                            "session settings schema unavailable for backend {:?}",
                            backend_kind
                        )
                    });
                    sanitize_session_settings_values(schema, &stored_settings)
                });
                let backend_support_failure = (!use_mock_backend
                    && !matches!(
                        backend_kind,
                        protocol::BackendKind::Claude | protocol::BackendKind::Codex
                    ))
                .then(|| {
                    AgentStartupFailure::unsupported(
                        crate::backend::backend_fork_unsupported_message(backend_kind),
                    )
                });
                let non_resumable_failure = (!session_record_is_resumable(&record)).then(|| {
                    AgentStartupFailure::unsupported(format!(
                        "cannot fork non-resumable session {}",
                        from_session_id
                    ))
                });
                let parent_agent_mismatch_failure = match parent_session_id.as_ref() {
                    Some(session_id) if session_id != &from_session_id => {
                        Some(AgentStartupFailure::internal(format!(
                            "fork parent_agent_id maps to session {}, not from_session_id {}",
                            session_id, from_session_id
                        )))
                    }
                    Some(_) => None,
                    None => parent_session_lookup_failure.clone(),
                };
                let startup_failure = startup_failure
                    .or(parent_agent_mismatch_failure)
                    .or(non_resumable_failure)
                    .or(backend_support_failure);
                let combined_startup_warning = match (startup_warning, missing_project_warning) {
                    (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let provisional = format!("BTW: {}", derive_agent_name(&prompt));
                        (
                            provisional.clone(),
                            Some(InitialAgentAlias {
                                name: provisional,
                                persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                            }),
                        )
                    }
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin: AgentOrigin::SideQuestion,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: None,
                    team_member_id: None,
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id: Some(from_session_id.clone()),
                    project_id,
                    backend_kind,
                    workspace_roots,
                    initial_input: Some(protocol::SendMessagePayload {
                        message: prompt,
                        images,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint: None,
                    session_settings: sanitized_settings,
                    session_settings_schema,
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: None,
                    fork_from_session_id: Some(from_session_id),
                    startup_warning: combined_startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
        };

        let request = self.apply_complexity_tier_settings(request).await;
        tracing::info!(
            backend_kind = ?request.backend_kind,
            workspace_roots = ?request.workspace_roots,
            startup_mcp_servers = request.startup_mcp_servers.len(),
            resume_session_id = ?request.resume_session_id,
            fork_from_session_id = ?request.fork_from_session_id,
            "host spawn_agent resolved request"
        );

        let (start, agent_handle, startup_rx, host_streams) = {
            let mut state = self.state.lock().await;
            let sub_agent_spawn_tx = state.sub_agent_spawn_tx.clone();
            let review_registry = state.review_registry.clone();
            let spawned = state.registry.spawn(
                request,
                Arc::clone(&session_store),
                sub_agent_spawn_tx,
                review_registry,
            );
            let host_streams = state
                .host_streams
                .iter()
                .map(|(path, subscriber)| (path.clone(), subscriber.stream.clone()))
                .collect::<Vec<_>>();
            (
                spawned.start,
                spawned.handle,
                spawned.startup_rx,
                host_streams,
            )
        };

        let mut dead_paths = Vec::new();
        for (path, stream) in host_streams {
            if emit_new_agent_for_stream(&start, &agent_handle, &stream)
                .await
                .is_err()
            {
                dead_paths.push(path);
            }
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
            }
        }

        let agent_id = start.agent_id.clone();
        self.schedule_agent_session_registration(agent_id.clone(), startup_rx);
        tracing::info!(
            agent_id = %agent_id,
            backend_kind = ?start.backend_kind,
            name = %start.name,
            "host spawn_agent completed"
        );

        if let Some(request) = deferred_generated_name {
            self.schedule_generated_agent_name(agent_id.clone(), request);
        }

        agent_id
    }

    /// Applies the host-level "task complexity tiers" setting to a spawn request.
    ///
    /// When tiers are disabled (the default), the cost hint is dropped so every
    /// spawn uses the backend's own defaults. When enabled, a Low/High hint
    /// resolves through the user's per-backend tier config when one exists
    /// (explicit session settings still win); backends without a config fall
    /// back to their built-in mapping. Medium is a legacy no-op.
    async fn apply_complexity_tier_settings(
        &self,
        mut request: ResolvedSpawnRequest,
    ) -> ResolvedSpawnRequest {
        let Some(hint) = request.cost_hint else {
            return request;
        };
        let settings_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.settings_store)
        };
        let settings = match settings_store.lock().await.get() {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(%error, "failed to read host settings; ignoring spawn cost hint");
                request.cost_hint = None;
                return request;
            }
        };
        if !settings.complexity_tiers_enabled {
            request.cost_hint = None;
            return request;
        }
        let tier_values = match (
            hint,
            settings.backend_tier_configs.get(&request.backend_kind),
        ) {
            (protocol::SpawnCostHint::Medium, _) => {
                request.cost_hint = None;
                return request;
            }
            // No user config for this backend: the backend's built-in
            // tier mapping applies via the cost hint as before.
            (_, None) => return request,
            (protocol::SpawnCostHint::Low, Some(config)) => config.low.clone(),
            (protocol::SpawnCostHint::High, Some(config)) => config.high.clone(),
        };
        let mut merged = match request.session_settings_schema.as_ref() {
            Some(schema) => sanitize_session_settings_values(schema, &tier_values),
            None => tier_values,
        };
        if let Some(explicit) = request.session_settings.take() {
            apply_session_settings_update(&mut merged, &explicit);
        }
        request.session_settings = (!merged.0.is_empty()).then_some(merged);
        request.cost_hint = None;
        request
    }

    async fn spawn_resolved_agent(&self, request: ResolvedSpawnRequest) -> AgentId {
        let request = self.apply_complexity_tier_settings(request).await;
        tracing::info!(
            backend_kind = ?request.backend_kind,
            workspace_roots = ?request.workspace_roots,
            startup_mcp_servers = request.startup_mcp_servers.len(),
            resume_session_id = ?request.resume_session_id,
            fork_from_session_id = ?request.fork_from_session_id,
            "host spawn_agent resolved request"
        );

        let session_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.session_store)
        };
        let (start, agent_handle, startup_rx, host_streams) = {
            let mut state = self.state.lock().await;
            let sub_agent_spawn_tx = state.sub_agent_spawn_tx.clone();
            let review_registry = state.review_registry.clone();
            let spawned =
                state
                    .registry
                    .spawn(request, session_store, sub_agent_spawn_tx, review_registry);
            let host_streams = state
                .host_streams
                .iter()
                .map(|(path, subscriber)| (path.clone(), subscriber.stream.clone()))
                .collect::<Vec<_>>();
            (
                spawned.start,
                spawned.handle,
                spawned.startup_rx,
                host_streams,
            )
        };

        let mut dead_paths = Vec::new();
        for (path, stream) in host_streams {
            if emit_new_agent_for_stream(&start, &agent_handle, &stream)
                .await
                .is_err()
            {
                dead_paths.push(path);
            }
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
            }
        }

        let agent_id = start.agent_id.clone();
        self.schedule_agent_session_registration(agent_id.clone(), startup_rx);
        tracing::info!(
            agent_id = %agent_id,
            backend_kind = ?start.backend_kind,
            name = %start.name,
            "host spawn_agent completed"
        );
        agent_id
    }

    pub(crate) async fn create_project(&self, payload: ProjectCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "project_create";
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .create(payload.name, payload.roots)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        if let Err(error) = ensure_project_actor(&mut state, project_id.clone()).await {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to start project actor after project creation"
            );
        }
        let host_paths = state.host_streams.keys().cloned().collect::<Vec<_>>();
        for host_path in host_paths {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to project stream after project creation"
                );
            }
            if let Err(error) =
                emit_review_list_changed_for_project(&mut state, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to emit initial review list after project creation"
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn rename_project(&self, payload: ProjectRenamePayload) -> AppResult<()> {
        const OPERATION: &str = "project_rename";
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .rename(&payload.id, payload.name)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        Ok(())
    }

    pub(crate) async fn reorder_projects(&self, payload: ProjectReorderPayload) -> AppResult<()> {
        const OPERATION: &str = "project_reorder";
        let mut state = self.state.lock().await;
        let projects = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .reorder(payload.scope, payload.project_ids)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        for project in projects {
            fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        }
        Ok(())
    }

    pub(crate) async fn add_project_root(&self, payload: ProjectAddRootPayload) -> AppResult<()> {
        const OPERATION: &str = "project_add_root";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .add_root(&payload.id, payload.root)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to start project actor after adding project root"
                );
                None
            }
        };
        drop(state);
        if let Some(handle) = handle
            && let Err(error) = handle.refresh().await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to refresh project actor after adding project root"
            );
        }
        Ok(())
    }

    pub(crate) async fn delete_project_root(
        &self,
        payload: ProjectDeleteRootPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_delete_root";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .delete_root(&payload.id, &payload.root)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to start project actor after deleting project root"
                );
                None
            }
        };
        drop(state);
        if let Some(handle) = handle
            && let Err(error) = handle.refresh().await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to refresh project actor after deleting project root"
            );
        }
        Ok(())
    }

    pub(crate) async fn delete_project(&self, payload: ProjectDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "project_delete";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        {
            let project_store = state.project_store.lock().await;
            let Some(project) = project_store.get(&payload.id) else {
                return Err(AppError::not_found(
                    OPERATION,
                    format!("cannot delete missing project {}", payload.id),
                ));
            };
            if project.is_workbench() {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "cannot delete workbench project {} with ProjectDelete; use WorkbenchRemove",
                        payload.id
                    ),
                ));
            }
            if let Some(child) = project_store.list_children(&payload.id).first() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot delete project {} while referenced by workbench {}",
                        payload.id, child
                    ),
                ));
            }
        }
        let deleted_steering_ids = state
            .steering_store
            .lock()
            .await
            .delete_for_project(&payload.id)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let detached_session_ids = state
            .session_store
            .lock()
            .await
            .detach_project(&payload.id)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let team_refs = agent_team_validation_refs(&state, OPERATION).await?;
        let team_events = state
            .team_registry
            .remove_project_refs(payload.id.clone(), team_refs)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .delete(&payload.id)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        cleanup_reviews_for_deleted_project(&state.review_registry, &payload.id).await;
        if let Some(subscription) = state.project_streams.remove(&payload.id) {
            subscription.task.abort();
        }
        for id in deleted_steering_ids {
            fan_out_steering_notify(&mut state, SteeringNotifyPayload::Delete { id }).await;
        }
        fan_out_team_registry_events(&mut state, team_events).await;
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Delete { project }).await;
        if !detached_session_ids.is_empty() {
            fan_out_session_lists(&mut state).await;
        }
        Ok(())
    }

    pub(crate) async fn create_workbench(&self, payload: WorkbenchCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "workbench_create";
        let parent_lock = self.workbench_parent_lock(&payload.parent_project_id).await;
        let _parent_guard = parent_lock.lock().await;

        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };

        let parent = {
            let project_store = project_store.lock().await;
            project_store
                .get(&payload.parent_project_id)
                .ok_or_else(|| {
                    AppError::not_found(
                        OPERATION,
                        format!("project {} not found", payload.parent_project_id),
                    )
                })?
        };

        let ProjectSource::Standalone {
            roots: parent_roots,
        } = &parent.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot create workbench for non-standalone parent project {}",
                    parent.id
                ),
            ));
        };

        if parent_roots.is_empty() {
            return Err(AppError::internal_message(
                OPERATION,
                format!("standalone parent project {} has no roots", parent.id),
                anyhow!("standalone parent project has no roots"),
            ));
        }

        let branch = payload.branch.clone();
        let roots = compute_workbench_roots(parent_roots, &branch)?;
        preflight_workbench_create(&project_store, &parent, &branch, &roots).await?;

        let mut created = Vec::<WorkbenchRoot>::new();
        for root in &roots {
            if let Err(error) =
                git_worktree_add(&root.parent_root, &root.worktree_root, &branch).await
            {
                let rollback_message = rollback_created_worktrees(&created, &branch).await;
                let message = append_rollback_message(error, rollback_message);
                return Err(AppError::internal_message(
                    OPERATION,
                    message.clone(),
                    anyhow!(message),
                ));
            }
            created.push(root.clone());
        }

        let project = {
            let mut project_store = project_store.lock().await;
            match project_store.create_workbench(
                payload.parent_project_id.clone(),
                payload.name,
                payload.branch,
                roots,
            ) {
                Ok(project) => project,
                Err(error) => {
                    let rollback_message = rollback_created_worktrees(&created, &branch).await;
                    let message = append_rollback_message(error.to_string(), rollback_message);
                    return Err(project_store_error(OPERATION, error.with_message(message)));
                }
            }
        };

        let project_id = project.id.clone();
        let mut state = self.state.lock().await;
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        // The workbench record and worktrees already exist and the Upsert
        // has fanned out, so a project-actor failure here must not fail
        // the request (mirrors create_project): warn and return success.
        if let Err(error) = ensure_project_actor(&mut state, project_id.clone()).await {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to start project actor after workbench creation"
            );
        }
        let host_paths = state.host_streams.keys().cloned().collect::<Vec<_>>();
        for host_path in host_paths {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to workbench project stream after creation"
                );
            }
            if let Err(error) =
                emit_review_list_changed_for_project(&mut state, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to emit initial review list after workbench creation"
                );
            }
        }

        Ok(())
    }

    pub(crate) async fn remove_workbench(&self, payload: WorkbenchRemovePayload) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };

        let project = load_project(&project_store, &payload.id, OPERATION).await?;
        let ProjectSource::GitWorkbench {
            parent_project_id,
            roots,
            ..
        } = &project.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot remove non-workbench project {} as workbench",
                    payload.id
                ),
            ));
        };
        let parent_lock = self.workbench_parent_lock(parent_project_id).await;
        let _parent_guard = parent_lock.lock().await;

        // Mark the workbench as being removed before validating blockers
        // so agent spawns and terminal creation reject it while the slow
        // git subprocesses below run outside the host state lock. The
        // marker is cleared on every exit path (success or failure) by
        // the wrapper below.
        {
            let mut state = self.state.lock().await;
            if !state.removing_projects.insert(payload.id.clone()) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("workbench {} is already being removed", payload.id),
                ));
            }
        }
        let result = self
            .remove_workbench_marked(&payload, &project, roots, &project_store)
            .await;
        self.state
            .lock()
            .await
            .removing_projects
            .remove(&payload.id);
        result
    }

    async fn remove_workbench_marked(
        &self,
        payload: &WorkbenchRemovePayload,
        project: &Project,
        roots: &[WorkbenchRoot],
        project_store: &Arc<Mutex<ProjectStore>>,
    ) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        self.validate_workbench_remove_blockers(project).await?;

        for root in roots {
            let exists = tokio::fs::try_exists(&root.worktree_root.0)
                .await
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            if exists {
                git_worktree_remove(&root.parent_root, &root.worktree_root)
                    .await
                    .map_err(|error| {
                        AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                    })?;
            } else {
                // The worktree dir was deleted out of band (or by an
                // earlier half-failed removal); prune git's worktree
                // bookkeeping in the parent repo and keep going so the
                // record can still be removed. A retry after a partial
                // failure lands here and succeeds.
                tracing::warn!(
                    project_id = %project.id,
                    worktree_root = %root.worktree_root,
                    "worktree root missing during workbench removal; pruning git worktree bookkeeping"
                );
                if let Err(error) = git_worktree_prune(&root.parent_root).await {
                    tracing::warn!(
                        project_id = %project.id,
                        parent_root = %root.parent_root,
                        error = %error,
                        "failed to prune git worktrees for missing worktree root"
                    );
                }
            }
        }

        let deleted = {
            let mut project_store = project_store.lock().await;
            project_store
                .delete_workbench(&payload.id)
                .map_err(|error| project_store_error(OPERATION, error))?
        };

        let mut state = self.state.lock().await;
        cleanup_reviews_for_deleted_project(&state.review_registry, &payload.id).await;
        if let Some(subscription) = state.project_streams.remove(&payload.id) {
            subscription.task.abort();
        }
        fan_out_project_notify(
            &mut state,
            ProjectNotifyPayload::Delete { project: deleted },
        )
        .await;
        Ok(())
    }

    async fn validate_workbench_remove_blockers(&self, project: &Project) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        let ProjectSource::GitWorkbench {
            parent_project_id,
            roots,
            ..
        } = &project.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot remove non-workbench project {} as workbench",
                    project.id
                ),
            ));
        };

        let (
            agent_handles,
            terminal_handles,
            session_store,
            steering_store,
            project_store,
            team_registry,
        ) = {
            let state = self.state.lock().await;
            let agent_handles = state
                .registry
                .agent_ids()
                .into_iter()
                .filter_map(|agent_id| state.registry.agent_handle(&agent_id))
                .collect::<Vec<_>>();
            let terminal_handles = state.terminal_streams.values().cloned().collect::<Vec<_>>();
            (
                agent_handles,
                terminal_handles,
                Arc::clone(&state.session_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.project_store),
                state.team_registry.clone(),
            )
        };

        for agent in agent_handles {
            let start = agent.snapshot();
            if start.project_id.as_ref() == Some(&project.id) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot remove workbench {} while agent {} is live",
                        project.id, start.agent_id
                    ),
                ));
            }
        }

        for terminal in terminal_handles {
            if terminal.project_id() == Some(&project.id) && terminal.is_running() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot remove workbench {} while a terminal is live",
                        project.id
                    ),
                ));
            }
        }

        let referenced_session = session_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|session| session.project_id.as_ref() == Some(&project.id));
        if let Some(session) = referenced_session {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} while referenced by session {}",
                    project.id, session.id
                ),
            ));
        }

        let referenced_steering = steering_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|steering| matches!(&steering.scope, SteeringScope::Project(project_id) if project_id == &project.id));
        if let Some(steering) = referenced_steering {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} while referenced by steering {}",
                    project.id, steering.id
                ),
            ));
        }

        let snapshot = team_registry
            .snapshot()
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let referenced_team_members = snapshot
            .members
            .iter()
            .filter(|member| member.project_ids.contains(&project.id))
            .map(|member| member.id.clone())
            .collect::<Vec<_>>();
        if !referenced_team_members.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                referenced_team_member_delete_message(
                    "workbench",
                    &project.id,
                    Some(project.name.as_str()),
                    &snapshot,
                    &referenced_team_members,
                ),
            ));
        }

        let parent = {
            let project_store = project_store.lock().await;
            project_store.get(parent_project_id).ok_or_else(|| {
                AppError::internal_message(
                    OPERATION,
                    format!(
                        "workbench {} references missing parent project {}",
                        project.id, parent_project_id
                    ),
                    anyhow!("workbench parent record is missing"),
                )
            })?
        };
        let ProjectSource::Standalone {
            roots: parent_roots,
        } = &parent.source
        else {
            return Err(AppError::internal_message(
                OPERATION,
                format!(
                    "workbench {} parent project {} is not standalone",
                    project.id, parent_project_id
                ),
                anyhow!("workbench parent record is not standalone"),
            ));
        };
        let parent_root_set = parent_roots.iter().cloned().collect::<HashSet<_>>();

        for root in roots {
            if !parent_root_set.contains(&root.parent_root) {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!(
                        "workbench {} parent root {} is missing from parent project {}",
                        project.id, root.parent_root, parent_project_id
                    ),
                    anyhow!("workbench parent root is missing"),
                ));
            }
        }

        let mut dirty_roots = Vec::new();
        for root in roots {
            let exists = tokio::fs::try_exists(&root.worktree_root.0)
                .await
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            if !exists {
                // The worktree dir was deleted out of band (or by an
                // earlier half-failed removal). Treat it as removable
                // rather than blocking: removal prunes git bookkeeping
                // and deletes the record, so retries are the recourse.
                continue;
            }
            let status = git_status_porcelain(&root.worktree_root)
                .await
                .map_err(|error| {
                    AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                })?;
            if !status.trim().is_empty() {
                dirty_roots.push(root.worktree_root.0.clone());
            }
        }
        if !dirty_roots.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} because worktree roots are dirty: {}",
                    project.id,
                    dirty_roots.join(", ")
                ),
            ));
        }

        Ok(())
    }

    pub(crate) async fn upsert_custom_agent(
        &self,
        payload: CustomAgentUpsertPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "custom_agent_upsert";
        let mut state = self.state.lock().await;
        let custom_agent_id = payload.custom_agent.id.clone();
        let skills = state
            .skill_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let skill_ids = skills
            .into_iter()
            .map(|skill| skill.id)
            .collect::<HashSet<_>>();
        for skill_id in &payload.custom_agent.skill_ids {
            if !skill_ids.contains(skill_id) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "custom agent {} references missing skill {}",
                        custom_agent_id, skill_id
                    ),
                ));
            }
        }

        let mcp_servers = state
            .mcp_server_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let mcp_server_ids = mcp_servers
            .into_iter()
            .map(|mcp_server| mcp_server.id)
            .collect::<HashSet<_>>();
        for mcp_server_id in &payload.custom_agent.mcp_server_ids {
            if !mcp_server_ids.contains(mcp_server_id) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "custom agent {} references missing MCP server {}",
                        custom_agent_id, mcp_server_id
                    ),
                ));
            }
        }

        let custom_agent = state
            .custom_agent_store
            .lock()
            .await
            .upsert(payload.custom_agent)
            .map_err(|error| custom_agent_store_error(OPERATION, error))?;
        fan_out_custom_agent_notify(
            &mut state,
            CustomAgentNotifyPayload::Upsert { custom_agent },
        )
        .await;
        Ok(())
    }

    pub(crate) async fn delete_custom_agent(
        &self,
        payload: CustomAgentDeletePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "custom_agent_delete";
        let mut state = self.state.lock().await;
        let snapshot = state
            .team_registry
            .snapshot()
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        // Built-in team custom agents are wired into the role-preset
        // catalog as the default agent for each role. If we let those
        // records be deleted, the catalog would advertise stale ids that
        // drafts/templates would happily copy into draft members. Keep
        // invalid states unrepresentable by rejecting the delete.
        if snapshot
            .catalog
            .role_presets
            .iter()
            .any(|role| role.default_custom_agent_id.as_ref() == Some(&payload.id))
        {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete built-in custom agent {} that backs a team role preset",
                    payload.id
                ),
            ));
        }
        let referenced_team_members = snapshot
            .members
            .iter()
            .filter(|member| member.custom_agent_id.as_ref() == Some(&payload.id))
            .map(|member| member.id.clone())
            .collect::<Vec<_>>();
        if !referenced_team_members.is_empty() {
            let custom_agent_name = state
                .custom_agent_store
                .lock()
                .await
                .get(&payload.id)
                .map(|custom_agent| custom_agent.name);
            return Err(AppError::conflict(
                OPERATION,
                referenced_team_member_delete_message(
                    "custom agent",
                    &payload.id,
                    custom_agent_name.as_deref(),
                    &snapshot,
                    &referenced_team_members,
                ),
            ));
        }
        let id = state
            .custom_agent_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| custom_agent_store_error(OPERATION, error))?;
        fan_out_custom_agent_notify(&mut state, CustomAgentNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn upsert_steering(&self, payload: SteeringUpsertPayload) -> AppResult<()> {
        const OPERATION: &str = "steering_upsert";
        let mut state = self.state.lock().await;
        if let SteeringScope::Project(project_id) = &payload.steering.scope
            && !project_exists(&state.project_store, project_id, OPERATION).await?
        {
            return Err(AppError::not_found(
                OPERATION,
                format!(
                    "cannot upsert project-scoped steering {} for missing project {}",
                    payload.steering.id, project_id
                ),
            ));
        }
        let steering = state
            .steering_store
            .lock()
            .await
            .upsert(payload.steering)
            .map_err(|error| steering_store_error(OPERATION, error))?;
        fan_out_steering_notify(&mut state, SteeringNotifyPayload::Upsert { steering }).await;
        Ok(())
    }

    pub(crate) async fn delete_steering(&self, payload: SteeringDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "steering_delete";
        let mut state = self.state.lock().await;
        let id = state
            .steering_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| steering_store_error(OPERATION, error))?;
        fan_out_steering_notify(&mut state, SteeringNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn refresh_skills(&self, _payload: SkillRefreshPayload) -> AppResult<()> {
        const OPERATION: &str = "skill_refresh";
        let mut state = self.state.lock().await;
        let sync = state
            .skill_store
            .lock()
            .await
            .sync_from_disk()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        for id in sync.deletes {
            fan_out_skill_notify(&mut state, SkillNotifyPayload::Delete { id }).await;
        }
        for skill in sync.upserts {
            fan_out_skill_notify(&mut state, SkillNotifyPayload::Upsert { skill }).await;
        }
        Ok(())
    }

    pub(crate) async fn upsert_skill(&self, skill: Skill, body: String) -> AppResult<()> {
        const OPERATION: &str = "skill_upsert";
        let mut state = self.state.lock().await;
        let skill = state
            .skill_store
            .lock()
            .await
            .upsert(skill, body)
            .map_err(|error| skill_store_error(OPERATION, error))?;
        fan_out_skill_notify(&mut state, SkillNotifyPayload::Upsert { skill }).await;
        Ok(())
    }

    pub(crate) async fn delete_skill(&self, id: protocol::SkillId) -> AppResult<()> {
        const OPERATION: &str = "skill_delete";
        let mut state = self.state.lock().await;
        let id = state
            .skill_store
            .lock()
            .await
            .delete(&id)
            .map_err(|error| skill_store_error(OPERATION, error))?;
        fan_out_skill_notify(&mut state, SkillNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn upsert_mcp_server(&self, payload: McpServerUpsertPayload) -> AppResult<()> {
        const OPERATION: &str = "mcp_server_upsert";
        let mut state = self.state.lock().await;
        let name = payload.mcp_server.name.trim();
        if RESERVED_MCP_SERVER_NAMES.contains(&name) {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "MCP server {} name '{}' is reserved",
                    payload.mcp_server.id, name
                ),
            ));
        }
        let mcp_server = state
            .mcp_server_store
            .lock()
            .await
            .upsert(payload.mcp_server)
            .map_err(|error| mcp_server_store_error(OPERATION, error))?;
        fan_out_mcp_server_notify(&mut state, McpServerNotifyPayload::Upsert { mcp_server }).await;
        Ok(())
    }

    pub(crate) async fn delete_mcp_server(&self, payload: McpServerDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "mcp_server_delete";
        let mut state = self.state.lock().await;
        let referenced_agent = state
            .custom_agent_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|custom_agent| custom_agent.mcp_server_ids.contains(&payload.id));
        if let Some(custom_agent) = referenced_agent {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete MCP server {} while it is referenced by custom agent {}",
                    payload.id, custom_agent.id
                ),
            ));
        }
        let id = state
            .mcp_server_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| mcp_server_store_error(OPERATION, error))?;
        fan_out_mcp_server_notify(&mut state, McpServerNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn describe_team_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<TeamDescribeData, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        registry.describe_for_agent(agent_id).await
    }

    pub(crate) async fn list_custom_agents(&self) -> Result<Vec<CustomAgent>, String> {
        let store = { Arc::clone(&self.state.lock().await.custom_agent_store) };
        let agents = store.lock().await.list()?;
        Ok(agents)
    }

    pub(crate) async fn list_skills(&self) -> Result<Vec<protocol::Skill>, String> {
        let store = { Arc::clone(&self.state.lock().await.skill_store) };
        let skills = store.lock().await.list()?;
        Ok(skills)
    }

    pub(crate) async fn list_mcp_servers(&self) -> Result<Vec<protocol::McpServerConfig>, String> {
        let store = { Arc::clone(&self.state.lock().await.mcp_server_store) };
        let servers = store.lock().await.list()?;
        Ok(servers)
    }

    pub(crate) async fn custom_agent_by_id(
        &self,
        id: &protocol::CustomAgentId,
    ) -> Result<Option<CustomAgent>, String> {
        let store = { Arc::clone(&self.state.lock().await.custom_agent_store) };
        Ok(store.lock().await.get(id))
    }

    /// User-initiated team-member activation (host stream). Mirrors the
    /// Reuse/Resume/New branches of `message_team_member` but skips the
    /// caller-is-manager auth. With `prompt: None`, the no-binding + no-session
    /// case is a no-op (just opens the chat tab; first user message will
    /// re-send with `prompt: Some`).
    pub(crate) async fn activate_team_member(
        &self,
        member_id: TeamMemberId,
        prompt: Option<String>,
        images: Option<Vec<ImageData>>,
    ) -> AppResult<TeamMemberMessageOutcome> {
        const OPERATION: &str = "team_member_activate";
        let registry = { self.state.lock().await.team_registry.clone() };
        let has_prompt = prompt.is_some();
        let plan = registry
            .plan_user_activation(member_id.clone(), has_prompt)
            .await
            .map_err(|error| team_member_activation_error(OPERATION, error))?;
        match plan.activation.clone() {
            TeamMemberActivation::Reuse { agent_id } => {
                if let Some(prompt) = prompt {
                    self.message_bound_team_member(&registry, &plan, agent_id, prompt, images)
                        .await
                        .map_err(|error| team_member_activation_error(OPERATION, error))
                } else {
                    Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id,
                        queued: false,
                    })
                }
            }
            TeamMemberActivation::Resume { session_id } => {
                if !has_prompt {
                    // Defer until a real message arrives.
                    return Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id: AgentId(String::new()),
                        queued: false,
                    });
                }
                if let Err(err) = self.ensure_team_resume_session(&session_id).await {
                    self.record_team_member_resume_failure(&registry, plan.member.id.clone())
                        .await
                        .map_err(|error| team_member_activation_error(OPERATION, error))?;
                    return Err(team_member_activation_error(OPERATION, err));
                }
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::Resume { session_id, prompt },
                )
                .await
                .map_err(|error| team_member_activation_error(OPERATION, error))
            }
            TeamMemberActivation::New => {
                let Some(message) = prompt else {
                    // Fresh member + no prompt: nothing to do server-side.
                    return Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id: AgentId(String::new()),
                        queued: false,
                    });
                };
                let prompt = if plan.member.role == TeamMemberRole::Manager
                    && plan.member.session_id.is_none()
                {
                    match self.manager_prompt_with_roster(&plan, message).await {
                        Ok(prompt) => prompt,
                        Err(err) => {
                            let events = registry
                                .record_binding_failure(plan.member.id.clone())
                                .await
                                .map_err(|error| team_member_activation_error(OPERATION, error))?;
                            self.fan_out_team_registry_events(events).await;
                            return Err(team_member_activation_error(OPERATION, err));
                        }
                    }
                } else {
                    message
                };
                let backend_kind = plan.member.backend_kind;
                let workspace_roots = match self.team_member_workspace_roots(&plan.member).await {
                    Ok(workspace_roots) => workspace_roots,
                    Err(err) => {
                        let events = registry
                            .record_binding_failure(plan.member.id.clone())
                            .await
                            .map_err(|error| team_member_activation_error(OPERATION, error))?;
                        self.fan_out_team_registry_events(events).await;
                        return Err(team_member_activation_error(OPERATION, err));
                    }
                };
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::New {
                        workspace_roots,
                        prompt,
                        images,
                        backend_kind,
                        cost_hint: plan.member.cost_hint,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                )
                .await
                .map_err(|error| team_member_activation_error(OPERATION, error))
            }
        }
    }

    pub(crate) async fn message_team_member(
        &self,
        caller_agent_id: AgentId,
        member_id: TeamMemberId,
        message: String,
        images: Option<Vec<ImageData>>,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        let plan = registry
            .plan_message_member(caller_agent_id, member_id.clone())
            .await?;
        match plan.activation.clone() {
            TeamMemberActivation::Reuse { agent_id } => {
                self.message_bound_team_member(&registry, &plan, agent_id, message, images)
                    .await
            }
            TeamMemberActivation::Resume { session_id } => {
                if let Err(err) = self.ensure_team_resume_session(&session_id).await {
                    self.record_team_member_resume_failure(&registry, plan.member.id.clone())
                        .await?;
                    return Err(err);
                }
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::Resume {
                        session_id,
                        prompt: Some(message),
                    },
                )
                .await
            }
            TeamMemberActivation::New => {
                let prompt = if plan.member.role == TeamMemberRole::Manager
                    && plan.member.session_id.is_none()
                {
                    match self.manager_prompt_with_roster(&plan, message).await {
                        Ok(prompt) => prompt,
                        Err(err) => {
                            let events = registry
                                .record_binding_failure(plan.member.id.clone())
                                .await?;
                            self.fan_out_team_registry_events(events).await;
                            return Err(err);
                        }
                    }
                } else {
                    message
                };
                let backend_kind = plan.member.backend_kind;
                let workspace_roots = match self.team_member_workspace_roots(&plan.member).await {
                    Ok(workspace_roots) => workspace_roots,
                    Err(err) => {
                        let events = registry
                            .record_binding_failure(plan.member.id.clone())
                            .await?;
                        self.fan_out_team_registry_events(events).await;
                        return Err(err);
                    }
                };
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::New {
                        workspace_roots,
                        prompt,
                        images,
                        backend_kind,
                        cost_hint: plan.member.cost_hint,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                )
                .await
            }
        }
    }

    async fn message_bound_team_member(
        &self,
        registry: &TeamRegistryHandle,
        plan: &TeamMessagePlan,
        agent_id: AgentId,
        message: String,
        images: Option<Vec<ImageData>>,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let handle = self.agent_handle(&agent_id).await.ok_or_else(|| {
            format!(
                "team member {} is bound to missing agent {agent_id}",
                plan.member.id
            )
        })?;
        let queued = self
            .agent_status_snapshot(&agent_id)
            .await
            .map(|status| status.is_active())
            .unwrap_or(false);
        let events = registry
            .record_member_activity(plan.member.id.clone(), AgentControlStatus::Thinking)
            .await?;
        self.fan_out_team_registry_events(events).await;
        let sent = handle
            .send_input(AgentInput::SendMessage(SendMessagePayload {
                message,
                images,
                origin: None,
                tool_response: None,
            }))
            .await;
        if !sent {
            let events = registry
                .record_binding_failure(plan.member.id.clone())
                .await?;
            self.fan_out_team_registry_events(events).await;
            return Err(format!(
                "team member {} agent backend is closed",
                plan.member.id
            ));
        }
        Ok(TeamMemberMessageOutcome {
            member_id: plan.member.id.clone(),
            agent_id,
            queued,
        })
    }

    async fn spawn_unbound_team_member(
        &self,
        registry: &TeamRegistryHandle,
        plan: &TeamMessagePlan,
        params: SpawnAgentParams,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let clear_session_on_failure = matches!(&params, SpawnAgentParams::Resume { .. });
        let payload = SpawnAgentPayload {
            name: Some(plan.member.name.clone()),
            custom_agent_id: plan.member.custom_agent_id.clone(),
            parent_agent_id: None,
            project_id: Some(team_member_primary_project_id(&plan.member)?),
            params,
        };
        let agent_id = self
            .spawn_agent_with_origin_config_and_team(
                payload,
                AgentOrigin::TeamMember,
                None,
                Some(TeamSpawnContext {
                    team_id: plan.team.id.clone(),
                    team_member_id: plan.member.id.clone(),
                }),
            )
            .await;
        match self.wait_for_agent_session_id_result(&agent_id).await {
            Ok(session_id) => {
                let refs = {
                    let state = self.state.lock().await;
                    agent_team_validation_refs(&state, "team_member_bind")
                        .await
                        .map_err(|err| err.to_string())?
                };
                let events = registry
                    .bind_member_agent(
                        plan.member.id.clone(),
                        agent_id.clone(),
                        Some(session_id),
                        refs,
                    )
                    .await?;
                self.fan_out_team_registry_events(events).await;
                if let Some(status) = self.agent_status_snapshot(&agent_id).await {
                    let events = if status.terminated {
                        registry.clear_binding_by_agent(agent_id.clone()).await?
                    } else {
                        registry
                            .record_agent_activity(agent_id.clone(), status.status())
                            .await?
                    };
                    self.fan_out_team_registry_events(events).await;
                }
                Ok(TeamMemberMessageOutcome {
                    member_id: plan.member.id.clone(),
                    agent_id,
                    queued: false,
                })
            }
            Err(err) => {
                if clear_session_on_failure {
                    self.record_team_member_resume_failure(registry, plan.member.id.clone())
                        .await?;
                } else {
                    let events = registry
                        .record_binding_failure(plan.member.id.clone())
                        .await?;
                    self.fan_out_team_registry_events(events).await;
                }
                Err(err)
            }
        }
    }

    async fn record_team_member_resume_failure(
        &self,
        registry: &TeamRegistryHandle,
        member_id: TeamMemberId,
    ) -> Result<(), String> {
        let refs = {
            let state = self.state.lock().await;
            agent_team_validation_refs(&state, "team_member_resume_failure")
                .await
                .map_err(|err| err.to_string())?
        };
        let events = registry.record_resume_failure(member_id, refs).await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    async fn wait_for_agent_session_id_result(
        &self,
        agent_id: &AgentId,
    ) -> Result<SessionId, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let (session_id, status_handle) = {
                let state = self.state.lock().await;
                (
                    state.agent_sessions.get(agent_id).cloned(),
                    state.registry.agent_status_handle(agent_id),
                )
            };
            if let Some(session_id) = session_id {
                return Ok(session_id);
            }
            if let Some(status_handle) = status_handle {
                let status = status_handle.snapshot().await;
                if status.terminated {
                    return Err(status.last_error.unwrap_or_else(|| {
                        format!("agent {agent_id} terminated before session binding")
                    }));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for team member agent {agent_id} session binding"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn ensure_team_resume_session(&self, session_id: &SessionId) -> Result<(), String> {
        let session_store = { Arc::clone(&self.state.lock().await.session_store) };
        let record = session_store
            .lock()
            .await
            .list()
            .map_err(|error| format!("failed to load sessions before team resume: {error}"))?
            .into_iter()
            .find(|record| record.id == *session_id)
            .ok_or_else(|| format!("cannot resume missing session {session_id}"))?;
        if !session_record_is_resumable(&record) {
            return Err(format!("cannot resume non-resumable session {session_id}"));
        }
        Ok(())
    }

    async fn team_member_workspace_roots(
        &self,
        member: &TeamMember,
    ) -> Result<Vec<String>, String> {
        if member.project_ids.is_empty() {
            return Err(format!("team member {} has no project_ids", member.id));
        }
        let project_store = { Arc::clone(&self.state.lock().await.project_store) };
        let project_store = project_store.lock().await;
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for project_id in &member.project_ids {
            let project = project_store.get(project_id).ok_or_else(|| {
                format!(
                    "team member {} references missing project {}",
                    member.id, project_id
                )
            })?;
            for root in project.root_paths() {
                let root = root.0;
                if seen.insert(root.clone()) {
                    roots.push(root);
                }
            }
        }
        Ok(roots)
    }

    async fn manager_prompt_with_roster(
        &self,
        plan: &TeamMessagePlan,
        prompt: String,
    ) -> Result<String, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        let snapshot = registry.snapshot().await?;
        let members = snapshot
            .members
            .into_iter()
            .filter(|member| member.team_id == plan.team.id)
            .collect::<Vec<_>>();
        Ok(prepend_manager_roster(&plan.team, &members, prompt))
    }

    pub(crate) async fn create_team(&self, payload: TeamCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_create";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.create_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn rename_team(&self, payload: TeamRenamePayload) -> AppResult<()> {
        const OPERATION: &str = "team_rename";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.rename_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn delete_team(&self, payload: TeamDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "team_delete";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.delete_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn set_team_manager(&self, payload: TeamSetManagerPayload) -> AppResult<()> {
        const OPERATION: &str = "team_set_manager";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.set_manager(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn create_team_member(
        &self,
        payload: TeamMemberCreatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_create";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.create_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn update_team_member(
        &self,
        payload: TeamMemberUpdatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_update";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.update_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn delete_team_member(
        &self,
        payload: TeamMemberDeletePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_delete";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.delete_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn create_team_draft(&self, payload: TeamDraftCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_create";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .create_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn update_team_draft(&self, payload: TeamDraftUpdatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_update";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .update_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn shuffle_team_draft(
        &self,
        payload: TeamDraftShufflePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_shuffle";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .shuffle_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn shuffle_team_member(
        &self,
        payload: TeamMemberShufflePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_shuffle";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .shuffle_member_suggestion(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn apply_team_draft_template(
        &self,
        payload: TeamDraftApplyTemplatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_apply_template";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .apply_draft_template(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn commit_team_draft(&self, payload: TeamDraftCommitPayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_commit";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.commit_draft(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn discard_team_draft(
        &self,
        payload: TeamDraftDiscardPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_discard";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .discard_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    async fn serialized_team_registry_mutation<F, Fut>(
        &self,
        operation: &'static str,
        mutate: F,
    ) -> AppResult<TeamRegistryEvents>
    where
        F: FnOnce(TeamRegistryHandle, AgentTeamValidationRefs) -> Fut,
        Fut: Future<Output = Result<TeamRegistryEvents, String>>,
    {
        let state = self.state.lock().await;
        let registry = state.team_registry.clone();
        let refs = agent_team_validation_refs(&state, operation).await?;
        #[cfg(test)]
        wait_for_team_mutation_after_refs_test_hook(self, operation).await;
        // Hold host_state through the registry mutation so the validation-ref
        // snapshot and persisted team change serialize with project/custom-agent deletes.
        let events = mutate(registry, refs)
            .await
            .map_err(|error| team_registry_error(operation, error))?;
        drop(state);
        Ok(events)
    }

    async fn fan_out_team_registry_events(&self, events: TeamRegistryEvents) {
        let mut state = self.state.lock().await;
        fan_out_team_registry_events(&mut state, events).await;
    }

    pub(crate) async fn list_sessions(&self, host_output_stream: &Stream) -> AppResult<()> {
        const OPERATION: &str = "list_sessions";
        let sessions = {
            let state = self.state.lock().await;
            let mut sessions = state
                .session_store
                .lock()
                .await
                .summaries()
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            normalize_antigravity_session_resumability(&mut sessions);
            sessions
        };

        let payload = SessionListPayload { sessions };
        let payload = serde_json::to_value(&payload).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize SessionList payload for host stream",
                error,
            )
        })?;
        let _ = host_output_stream.send_value(FrameKind::SessionList, payload);
        Ok(())
    }

    pub(crate) async fn delete_session(&self, session_id: SessionId) -> AppResult<()> {
        const OPERATION: &str = "delete_session";
        let session_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.session_store)
        };
        session_store
            .lock()
            .await
            .delete(&session_id)
            .map_err(|error| session_store_error(OPERATION, error))?;
        self.fan_out_session_lists().await;
        Ok(())
    }

    pub(crate) async fn fan_out_session_lists(&self) {
        let mut state = self.state.lock().await;
        fan_out_session_lists(&mut state).await;
    }

    pub(crate) async fn set_setting(&self, payload: SetSettingPayload) -> AppResult<()> {
        const OPERATION: &str = "set_setting";
        let mut state = self.state.lock().await;
        let refresh_session_schemas = matches!(
            &payload.setting,
            protocol::HostSettingValue::EnabledBackends { .. }
        );
        let settings = state
            .settings_store
            .lock()
            .await
            .apply(payload.setting)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        fan_out_host_settings(&mut state, settings.clone()).await;
        state.mobile_access.settings_changed(settings);
        if refresh_session_schemas {
            drop(state);
            self.refresh_session_schemas().await;
        }
        Ok(())
    }

    pub(crate) async fn start_mobile_pairing(&self, requester: StreamPath) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.start_pairing(requester)
    }

    pub(crate) async fn cancel_mobile_pairing(
        &self,
        payload: MobilePairingCancelPayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.cancel_pairing(payload)
    }

    pub(crate) async fn revoke_mobile_device(
        &self,
        payload: MobileDeviceRevokePayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.revoke_device(payload).await
    }

    pub(crate) async fn rename_mobile_device(
        &self,
        payload: MobileDeviceRenamePayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.rename_device(payload).await
    }

    pub(crate) async fn fan_out_backend_setup(&self) {
        let payload = setup::collect_backend_setup().await;
        let mut state = self.state.lock().await;
        fan_out_backend_setup(&mut state, payload).await;
    }

    fn schedule_session_schema_refresh(&self) {
        let host = self.clone();
        tokio::spawn(async move {
            host.refresh_session_schemas().await;
        });
    }

    pub(crate) async fn refresh_session_schemas(&self) {
        let (settings_store, kiro_probe_program) = {
            let mut state = self.state.lock().await;
            state.kiro_session_schema = KiroSessionSchemaState::Pending;
            (
                Arc::clone(&state.settings_store),
                state.kiro_probe_program.clone(),
            )
        };
        let enabled_backends = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for session schemas: {err}"))
            .enabled_backends;

        let kiro_session_schema = if enabled_backends.contains(&protocol::BackendKind::Kiro) {
            match kiro_probe_workspace_root() {
                Ok(workspace_root) => match crate::backend::kiro::probe_session_settings_schema(
                    &[workspace_root],
                    kiro_probe_program,
                )
                .await
                {
                    Ok(schema) => KiroSessionSchemaState::Ready(schema),
                    Err(err) => {
                        tracing::error!("failed to refresh Kiro session schema: {err}");
                        KiroSessionSchemaState::Unavailable(err)
                    }
                },
                Err(err) => {
                    tracing::error!("failed to resolve Kiro probe workspace root: {err}");
                    KiroSessionSchemaState::Unavailable(err)
                }
            }
        } else {
            KiroSessionSchemaState::Pending
        };

        let mut state = self.state.lock().await;
        state.kiro_session_schema = kiro_session_schema;
        fan_out_session_schemas(&mut state).await;
    }

    pub(crate) async fn run_backend_setup(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: RunBackendSetupPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "run_backend_setup";
        tracing::info!(
            connection_host_stream = %connection_host_stream,
            host_stream = %host_output_stream.path(),
            backend_kind = ?payload.backend_kind,
            action = ?payload.action,
            "host run_backend_setup requested"
        );
        let Some(command) = setup::runnable_command(payload.backend_kind, payload.action).await
        else {
            return Err(AppError::not_found(
                OPERATION,
                format!(
                    "no runnable backend setup command for {:?} {:?}",
                    payload.backend_kind, payload.action
                ),
            ));
        };

        let terminal = self
            .create_terminal_internal(
                connection_host_stream,
                host_output_stream,
                TerminalCreatePayload {
                    target: TerminalLaunchTarget::HostDefault,
                    cols: 100,
                    rows: 28,
                },
            )
            .await?;

        if let Some(terminal) = terminal {
            tracing::info!(
                backend_kind = ?payload.backend_kind,
                action = ?payload.action,
                command = %command,
                "host run_backend_setup launching terminal command"
            );
            terminal
                .send(TerminalSendPayload {
                    data: format!(
                        "{command}
"
                    ),
                })
                .await;

            let host = self.clone();
            let backend_kind = payload.backend_kind;
            let action = payload.action;
            tokio::spawn(async move {
                let exit = terminal.wait_for_exit().await;
                tracing::info!(
                    backend_kind = ?backend_kind,
                    action = ?action,
                    exit_code = exit.exit_code,
                    signal = exit.signal.as_deref(),
                    "host run_backend_setup terminal exited; refreshing backend setup"
                );
                host.fan_out_backend_setup().await;
                host.refresh_session_schemas().await;
            });
        }
        Ok(())
    }

    pub(crate) async fn create_terminal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: TerminalCreatePayload,
    ) -> AppResult<()> {
        let _ = self
            .create_terminal_internal(connection_host_stream, host_output_stream, payload)
            .await?;
        Ok(())
    }

    async fn create_terminal_internal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: TerminalCreatePayload,
    ) -> AppResult<Option<TerminalHandle>> {
        const OPERATION: &str = "terminal_create";
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let launch = resolve_terminal_launch(&project_store, payload).await?;
        let launch_project_id = launch.project_id.clone();
        if let Some(project_id) = launch_project_id.as_ref() {
            let state = self.state.lock().await;
            if state.removing_projects.contains(project_id) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot create terminal in workbench {} because it is being removed",
                        project_id
                    ),
                ));
            }
        }
        let terminal_id = TerminalId(Uuid::new_v4().to_string());
        let terminal_stream_path = StreamPath(format!("/terminal/{}", terminal_id));
        let terminal_output_stream = host_output_stream.with_path(terminal_stream_path.clone());
        let terminal = create_terminal(launch, terminal_output_stream)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;

        {
            let mut state = self.state.lock().await;
            // Re-check under the same lock that guards `terminal_streams`:
            // workbench removal inserts into `removing_projects` before it
            // validates blockers, so either this terminal is registered in
            // time for the blocker check to see it, or the removal marker
            // is visible here and the terminal is rejected.
            if let Some(project_id) = launch_project_id.as_ref()
                && state.removing_projects.contains(project_id)
            {
                drop(state);
                terminal.close().await;
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot create terminal in workbench {} because it is being removed",
                        project_id
                    ),
                ));
            }
            let previous = state.terminal_streams.insert(
                (connection_host_stream.clone(), terminal_id),
                terminal.clone(),
            );
            if previous.is_some() {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!(
                        "duplicate terminal registration for {}",
                        terminal_stream_path
                    ),
                    anyhow!("duplicate terminal registration"),
                ));
            }
        }

        let host_payload =
            serde_json::to_value(terminal.new_terminal_payload()).map_err(|error| {
                AppError::internal_message(
                    OPERATION,
                    "failed to serialize new terminal payload",
                    error,
                )
            })?;
        if host_output_stream
            .send_value(FrameKind::NewTerminal, host_payload)
            .is_err()
        {
            return Ok(None);
        }
        let _ = terminal.emit_bootstrap_and_start_io().await;
        Ok(Some(terminal))
    }

    pub(crate) async fn send_terminal_input(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalSendPayload,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.send(payload).await;
        Ok(())
    }

    pub(crate) async fn resize_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalResizePayload,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.resize(payload.cols, payload.rows).await;
        Ok(())
    }

    pub(crate) async fn close_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.close().await;
        Ok(())
    }

    pub(crate) async fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.state.lock().await.registry.agent_handle(agent_id)
    }

    pub(crate) async fn interrupt_agent(&self, agent_id: &AgentId) -> InterruptOutcome {
        let (parent_handle, candidate_handles) = {
            let state = self.state.lock().await;
            let Some(parent_handle) = state.registry.agent_handle(agent_id) else {
                return InterruptOutcome::NotRunning;
            };
            let candidate_handles = state
                .registry
                .agent_ids()
                .into_iter()
                .filter(|candidate_id| candidate_id != agent_id)
                .filter_map(|candidate_id| state.registry.agent_handle(&candidate_id))
                .collect::<Vec<_>>();
            (parent_handle, candidate_handles)
        };

        let mut tyde_owned_children = Vec::new();
        for handle in candidate_handles {
            let start = handle.snapshot();
            if start.parent_agent_id.as_ref() == Some(agent_id)
                && start.origin != AgentOrigin::BackendNative
            {
                tyde_owned_children.push(handle);
            }
        }

        let outcome = parent_handle.interrupt().await;
        for child in tyde_owned_children {
            let _ = child.interrupt().await;
        }

        outcome
    }

    pub(crate) async fn close_agent(&self, agent_id: &AgentId) -> bool {
        let (close_targets, host_streams) = {
            let state = self.state.lock().await;
            let close_targets = state.registry.agent_subtree_post_order(agent_id);
            if close_targets.is_empty() {
                return false;
            }
            let host_streams = state
                .host_streams
                .iter()
                .map(|(path, subscriber)| (path.clone(), subscriber.stream.clone()))
                .collect::<Vec<_>>();
            (close_targets, host_streams)
        };

        let close_ids = close_targets
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect::<Vec<_>>();
        let mut live_streams = host_streams;
        let mut dead_paths = Vec::new();

        for (target_agent_id, agent_handle) in close_targets {
            let _ = agent_handle.close().await;

            let payload = AgentClosedPayload {
                agent_id: target_agent_id,
            };
            let mut failed_paths = Vec::new();
            for (path, stream) in &live_streams {
                if emit_agent_closed_for_stream(&payload, stream)
                    .await
                    .is_err()
                {
                    failed_paths.push(path.clone());
                }
            }
            if !failed_paths.is_empty() {
                live_streams.retain(|(path, _)| !failed_paths.contains(path));
                dead_paths.extend(failed_paths);
            }
        }

        dead_paths.sort_by(|left, right| left.0.cmp(&right.0));
        dead_paths.dedup();

        let mut state = self.state.lock().await;
        for path in dead_paths {
            state.host_streams.remove(&path);
        }

        for closed_agent_id in close_ids {
            let removed = state.registry.remove_agent(&closed_agent_id);
            if removed.is_none() {
                tracing::debug!(
                    agent_id = %closed_agent_id,
                    "agent was already removed before close cleanup completed"
                );
                continue;
            }

            state.agent_sessions.remove(&closed_agent_id);
            match state
                .team_registry
                .clear_binding_by_agent(closed_agent_id.clone())
                .await
            {
                Ok(events) => fan_out_team_registry_events(&mut state, events).await,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %closed_agent_id,
                        error = %error,
                        "failed to clear team binding while closing agent"
                    );
                }
            }
        }
        fan_out_session_lists(&mut state).await;

        true
    }

    pub(crate) async fn agent_status_handle(
        &self,
        agent_id: &AgentId,
    ) -> Option<crate::agent::registry::AgentStatusHandle> {
        self.state
            .lock()
            .await
            .registry
            .agent_status_handle(agent_id)
    }

    pub(crate) async fn agent_access_mode(
        &self,
        agent_id: &AgentId,
    ) -> Option<protocol::BackendAccessMode> {
        self.state.lock().await.registry.agent_access_mode(agent_id)
    }

    pub(crate) async fn agent_status_snapshot(
        &self,
        agent_id: &AgentId,
    ) -> Option<crate::agent::registry::AgentStatus> {
        let handle = self
            .state
            .lock()
            .await
            .registry
            .agent_status_handle(agent_id)?;
        Some(handle.snapshot().await)
    }

    pub(crate) async fn list_agents(&self) -> Vec<protocol::AgentStartPayload> {
        let handles = {
            let state = self.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .filter_map(|agent_id| state.registry.agent_handle(&agent_id))
                .collect::<Vec<_>>()
        };

        let mut starts = Vec::with_capacity(handles.len());
        for handle in handles {
            starts.push(handle.snapshot());
        }
        starts
    }

    pub async fn agent_ids(&self) -> Vec<AgentId> {
        self.state.lock().await.registry.agent_ids()
    }

    pub(crate) async fn subscribe_agent_status_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.lock().await.registry.subscribe_status_changes()
    }

    pub(crate) async fn read_settings(&self) -> Result<protocol::HostSettings, String> {
        self.state.lock().await.settings_store.lock().await.get()
    }

    pub async fn agent_control_mcp_url(&self) -> String {
        self.state.lock().await.agent_control_mcp.url.clone()
    }

    pub async fn review_mcp_url(&self) -> String {
        self.state.lock().await.review_mcp.url.clone()
    }

    pub(crate) async fn propose_review_comment(
        &self,
        review_id: ReviewId,
        suggestion: protocol::ReviewSuggestedComment,
    ) -> Result<Result<protocol::ReviewSuggestionId, protocol::ReviewErrorPayload>, String> {
        let registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        registry.ai_suggestion(review_id, suggestion).await
    }

    /// Spawn an agent and return its AgentId (for use by agent-control MCP).
    pub(crate) async fn spawn_agent_and_return_id(&self, payload: SpawnAgentPayload) -> AgentId {
        self.spawn_agent_with_origin(payload, AgentOrigin::AgentControl)
            .await
    }

    fn schedule_agent_session_registration(
        &self,
        agent_id: AgentId,
        startup_rx: tokio::sync::oneshot::Receiver<Result<SessionId, String>>,
    ) {
        let host = self.clone();
        tokio::spawn(async move {
            match startup_rx.await {
                Ok(Ok(session_id)) => {
                    {
                        let mut state = host.state.lock().await;
                        state
                            .agent_sessions
                            .insert(agent_id.clone(), session_id.clone());
                    }
                    host.fan_out_session_lists().await;
                    host.deliver_submitted_reviews_for_session(session_id).await;
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        error = %err,
                        "agent startup failed before session registration"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "agent startup channel dropped before session registration"
                    );
                }
            }
        });
    }

    fn schedule_generated_agent_name(&self, agent_id: AgentId, request: GenerateAgentNameRequest) {
        let host = self.clone();
        tokio::spawn(async move {
            let generated = match generate_agent_name(request).await {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!(
                        "background agent name generation failed for {}: {}",
                        agent_id,
                        err
                    );
                    return;
                }
            };

            let Some(agent) = host.agent_handle(&agent_id).await else {
                return;
            };
            if agent.set_generated_name(generated).await == Some(true) {
                host.fan_out_session_lists().await;
            }
        });
    }

    async fn wait_for_parent_session_id(&self, parent_agent_id: &AgentId) -> SessionId {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(session_id) = self
                .state
                .lock()
                .await
                .agent_sessions
                .get(parent_agent_id)
                .cloned()
            {
                return session_id;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "cannot resolve parent session for backend-native child {}",
                parent_agent_id
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn spawn_backend_native_subagent(
        &self,
        request: &HostSubAgentSpawnRequest,
    ) -> SubAgentHandle {
        tracing::info!(
            parent_agent_id = %request.parent_agent_id,
            tool_use_id = %request.tool_use_id,
            requested_name = %request.name,
            description = %request.description,
            agent_type = %request.agent_type,
            "host backend-native sub-agent spawn requested"
        );

        let (session_store, parent_handle) = {
            let state = self.state.lock().await;
            let parent_handle = state
                .registry
                .agent_handle(&request.parent_agent_id)
                .unwrap_or_else(|| {
                    panic!(
                        "cannot resolve parent agent {} for backend-native child",
                        request.parent_agent_id
                    )
                });
            (Arc::clone(&state.session_store), parent_handle)
        };
        let parent_session_id = self
            .wait_for_parent_session_id(&request.parent_agent_id)
            .await;

        let parent_start = parent_handle.snapshot();
        assert_eq!(
            parent_start.workspace_roots, request.workspace_roots,
            "backend-native child workspace roots must match parent {}",
            request.parent_agent_id
        );

        let session_id = request
            .session_id_hint
            .clone()
            .unwrap_or_else(|| SessionId(Uuid::new_v4().to_string()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let relay_request = RelaySpawnRequest {
            name: request.name.clone(),
            origin: AgentOrigin::BackendNative,
            custom_agent_id: parent_start.custom_agent_id.clone(),
            parent_agent_id: parent_start.agent_id.clone(),
            project_id: parent_start.project_id.clone(),
            backend_kind: parent_start.backend_kind,
            workspace_roots: parent_start.workspace_roots.clone(),
            session_id: session_id.clone(),
        };

        let (start, agent_handle, host_streams) = {
            let mut state = self.state.lock().await;
            let spawned =
                state
                    .registry
                    .spawn_relay(relay_request, event_rx, Arc::clone(&session_store));
            state
                .agent_sessions
                .insert(spawned.start.agent_id.clone(), session_id.clone());
            let host_streams = state
                .host_streams
                .iter()
                .map(|(path, subscriber)| (path.clone(), subscriber.stream.clone()))
                .collect::<Vec<_>>();
            (spawned.start, spawned.handle, host_streams)
        };

        session_store
            .lock()
            .await
            .upsert_backend_session(
                &BackendSession {
                    id: session_id.clone(),
                    backend_kind: start.backend_kind,
                    workspace_roots: start.workspace_roots.clone(),
                    title: Some(start.name.clone()),
                    token_count: None,
                    created_at_ms: Some(start.created_at_ms),
                    updated_at_ms: Some(start.created_at_ms),
                    resumable: false,
                },
                Some(parent_session_id),
                start.project_id.clone(),
                start.custom_agent_id.clone(),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "failed to persist backend-native child session {}: {err}",
                    session_id
                )
            });

        let mut dead_paths = Vec::new();
        for (path, stream) in host_streams {
            if emit_new_agent_for_stream(&start, &agent_handle, &stream)
                .await
                .is_err()
            {
                dead_paths.push(path);
            }
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
            }
        }

        self.fan_out_session_lists().await;

        SubAgentHandle {
            event_tx,
            agent_id: start.agent_id,
        }
    }

    pub(crate) async fn open_browse_stream(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: HostBrowseStartPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "host_browse_start";
        let browse_stream_path = payload.browse_stream;
        if !browse_stream_path.0.starts_with("/browse/") {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "browse stream must start with /browse/, got {}",
                    browse_stream_path
                ),
            ));
        }
        let home = browse_stream::home_dir();
        let initial = self.resolve_browse_initial(payload.initial, &home).await;
        let browse_stream = host_output_stream.with_path(browse_stream_path.clone());

        {
            let mut state = self.state.lock().await;
            let previous = state.browse_streams.insert(
                (connection_host_stream.clone(), browse_stream_path.clone()),
                browse_stream.clone(),
            );
            if previous.is_some() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "duplicate browse stream registration for {}",
                        browse_stream_path
                    ),
                ));
            }
        }

        let opened = browse_stream::opened_payload(&home);
        let listing = match browse_stream::list_dir(&initial, payload.include_hidden).await {
            Ok(entries) => BrowseBootstrapListing::Entries { entries },
            Err(error) => BrowseBootstrapListing::Error { error },
        };
        let payload =
            serde_json::to_value(BrowseBootstrapPayload { opened, listing }).map_err(|error| {
                AppError::internal_message(
                    OPERATION,
                    "failed to serialize BrowseBootstrap payload",
                    error,
                )
            })?;
        let _ = browse_stream.send_value(FrameKind::BrowseBootstrap, payload);
        Ok(())
    }

    /// Resolve the client's browse-start intent to a concrete directory.
    /// `ProjectRoots` falls back to home when the project is unknown or its
    /// roots have no useful common ancestor — the browser should still open.
    async fn resolve_browse_initial(
        &self,
        initial: HostBrowseInitial,
        home: &HostAbsPath,
    ) -> HostAbsPath {
        match initial {
            HostBrowseInitial::Home => home.clone(),
            HostBrowseInitial::Path { path } => path,
            HostBrowseInitial::ProjectRoots { project_id } => {
                let project = {
                    let state = self.state.lock().await;
                    let project_store = state.project_store.lock().await;
                    project_store.get(&project_id)
                };
                project
                    .and_then(|project| {
                        browse_stream::project_roots_initial_path(&project.root_paths())
                    })
                    .unwrap_or_else(|| home.clone())
            }
        }
    }

    pub(crate) async fn list_browse_dir(
        &self,
        connection_host_stream: &StreamPath,
        browse_stream_path: &StreamPath,
        payload: HostBrowseListPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "host_browse_list";
        let browse_stream = {
            let state = self.state.lock().await;
            state
                .browse_streams
                .get(&(connection_host_stream.clone(), browse_stream_path.clone()))
                .cloned()
                .ok_or_else(|| {
                    AppError::not_found(
                        OPERATION,
                        format!(
                            "browse stream {} is not owned by host stream {}",
                            browse_stream_path, connection_host_stream
                        ),
                    )
                })?
        };

        match browse_stream::list_dir(&payload.path, payload.include_hidden).await {
            Ok(entries) => browse_stream::emit_entries(&browse_stream, &entries).await,
            Err(error) => browse_stream::emit_error(&browse_stream, &error).await,
        }
        Ok(())
    }

    pub(crate) async fn close_browse_stream(
        &self,
        connection_host_stream: &StreamPath,
        browse_stream_path: &StreamPath,
    ) {
        let mut state = self.state.lock().await;
        state
            .browse_streams
            .remove(&(connection_host_stream.clone(), browse_stream_path.clone()));
    }

    async fn ensure_host_project_subscription(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        operation: &'static str,
    ) -> AppResult<ProjectStreamHandle> {
        let mut state = self.state.lock().await;
        let summaries = state
            .review_registry
            .summaries(project_id.clone())
            .await
            .map_err(|error| project_command_error(operation, error))?;
        let handle = ensure_project_actor(&mut state, project_id)
            .await
            .map_err(|error| project_command_error(operation, error))?;
        handle
            .add_subscriber(
                connection_host_stream.clone(),
                project_output_stream.clone(),
                summaries,
            )
            .await
            .map_err(|error| project_command_error(operation, error))?;
        Ok(handle)
    }

    pub(crate) async fn read_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_read_file";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let contents = read_file(&project, payload)
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&contents).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project file contents payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectFileContents, payload);
        Ok(())
    }

    /// Run a project-wide text search. The walk is offloaded to a blocking
    /// task that streams one `ProjectSearchResults` frame per matching file and
    /// a final `ProjectSearchComplete` frame, so the serial connection loop is
    /// never blocked. A newer search (or a matching cancel) for the same
    /// project supersedes any in-flight walk.
    pub(crate) async fn search_project_files(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectSearchPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_search";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;

        // Register this search as the active one for the project and grab the
        // shared atomic the walk will poll for cancellation.
        let search_id = payload.search_id;
        let active = {
            let mut state = self.state.lock().await;
            let slot = state
                .project_search_ids
                .entry(project_id.clone())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)));
            slot.store(search_id, Ordering::SeqCst);
            Arc::clone(slot)
        };

        let output = project_output_stream.clone();
        tokio::task::spawn_blocking(move || {
            let cancelled = {
                let active = Arc::clone(&active);
                move || active.load(Ordering::SeqCst) != search_id
            };
            let emit = {
                let active = Arc::clone(&active);
                let output = output.clone();
                move |file: ProjectSearchFileResult| -> bool {
                    // Re-check the active id immediately before sending: a long
                    // single-file scan may have outlived a cancel / supersede
                    // since the last per-file `cancelled` poll. Don't ship
                    // results the client will discard (or worse, mis-attribute).
                    if active.load(Ordering::SeqCst) != search_id {
                        return false;
                    }
                    match serde_json::to_value(&ProjectSearchResultsPayload { search_id, file }) {
                        Ok(value) => output
                            .send_value(FrameKind::ProjectSearchResults, value)
                            .is_ok(),
                        Err(_) => false,
                    }
                }
            };

            let (summary, error) = match search_project(&project, &payload, emit, cancelled) {
                Ok(summary) => (summary, None),
                Err(message) => (SearchSummary::default(), Some(message)),
            };

            // Suppress the terminal frame if this search was superseded — the
            // newer search owns the stream now.
            if active.load(Ordering::SeqCst) != search_id {
                return;
            }

            let complete = ProjectSearchCompletePayload {
                search_id,
                total_files: summary.total_files,
                total_matches: summary.total_matches,
                truncated: summary.truncated,
                cancelled: summary.cancelled,
                error,
            };
            if let Ok(value) = serde_json::to_value(&complete) {
                let _ = output.send_value(FrameKind::ProjectSearchComplete, value);
            }
        });

        Ok(())
    }

    /// Cancel an in-flight project search if `search_id` is still the active
    /// one for the project. Newer searches are left untouched.
    pub(crate) async fn cancel_project_search(
        &self,
        project_id: ProjectId,
        payload: ProjectSearchCancelPayload,
    ) -> AppResult<()> {
        let state = self.state.lock().await;
        if let Some(slot) = state.project_search_ids.get(&project_id) {
            // Only clear if it still points at the search being cancelled, so a
            // newer search that already replaced it keeps running.
            let _ = slot.compare_exchange(payload.search_id, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        Ok(())
    }

    pub(crate) async fn list_project_dir(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectListDirPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_list_dir";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let listing = build_dir_listing(&project, &payload.root, &payload.path)
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&listing).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project dir listing payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectFileList, payload);
        Ok(())
    }

    pub(crate) async fn read_project_diff(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadDiffPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_read_diff";
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                OPERATION,
            )
            .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let diff = read_diff(&project, payload)
            .map_err(|error| project_command_error(OPERATION, error))?;
        handle
            .remember_diff_context_mode(
                connection_host_stream.clone(),
                ProjectDiffRequestKey {
                    root: diff.root.clone(),
                    scope: diff.scope,
                    path: diff.path.clone(),
                },
                diff.context_mode,
            )
            .await
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&diff).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project git diff payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectGitDiff, payload);
        Ok(())
    }

    pub(crate) async fn stage_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_stage_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        stage_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn stage_project_hunk(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageHunkPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_stage_hunk";
        let path = payload.path;
        let hunk_id = payload.hunk_id;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        stage_hunk(&project, &path, &hunk_id)
            .map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn unstage_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectUnstageFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_unstage_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        unstage_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn discard_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectDiscardFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_discard_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        discard_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn commit_project(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectGitCommitPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_git_commit";
        let root = payload.root;
        let message = payload.message;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let commit_hash = commit(&project, &root, &message)
            .map_err(|error| project_command_error(OPERATION, error))?;
        let result_payload = ProjectGitCommitResultPayload {
            root: root.clone(),
            commit_hash,
        };
        let result_payload = serde_json::to_value(&result_payload).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project git commit result payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectGitCommitResult, result_payload);
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            None,
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn create_review(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ReviewCreatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_create";
        tracing::info!(
            project_id = %project_id,
            selection_kind = payload.selection.kind_name(),
            connection_stream = %connection_host_stream,
            output_stream = %project_output_stream.path(),
            "received review_create"
        );
        let (project_store, review_registry) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.project_store),
                state.review_registry.clone(),
            )
        };

        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let normalized_selection = review_create_selection(&project, &payload.selection)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let selection_root = match &normalized_selection {
            ReviewDiffSelection::Root { root, .. } => Some(root),
            ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => None,
        };

        let diff_started = Instant::now();
        tracing::debug!(
            project_id = %project_id,
            selection_kind = payload.selection.kind_name(),
            root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
            "reading initial review diffs"
        );
        let diffs = match read_review_diffs(&project, &normalized_selection) {
            Ok(diffs) => {
                let stats = host_diff_stats(&diffs);
                tracing::info!(
                    project_id = %project_id,
                    selection_kind = payload.selection.kind_name(),
                    root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
                    diff_count = stats.diff_count,
                    file_count = stats.file_count,
                    hunk_count = stats.hunk_count,
                    line_count = stats.line_count,
                    elapsed_ms = diff_started.elapsed().as_millis() as u64,
                    "read initial review diffs"
                );
                diffs
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    selection_kind = payload.selection.kind_name(),
                    root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
                    elapsed_ms = diff_started.elapsed().as_millis() as u64,
                    error_len = error.len(),
                    "failed to read initial review diffs"
                );
                return Err(AppError::internal_message(
                    OPERATION,
                    error.clone(),
                    anyhow!(error),
                ));
            }
        };
        let review_id = ReviewId(Uuid::new_v4().to_string());
        tracing::debug!(
            project_id = %project_id,
            review_id = %review_id,
            root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
            "creating or getting review actor"
        );
        let request = build_create_request(
            review_id.clone(),
            project_id.clone(),
            ReviewCreatePayload {
                selection: normalized_selection,
            },
            diffs,
        );
        let review_id = review_registry.create(request).await.map_err(|error| {
            AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
        })?;
        let review_stream = project_output_stream.with_path(review_stream_path(&review_id));
        review_registry
            .subscribe(
                review_id.clone(),
                connection_host_stream.clone(),
                review_stream,
                true,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        tracing::info!(review_id = %review_id, project_id = %project_id, "created or attached review");
        Ok(())
    }

    pub(crate) async fn review_action(
        &self,
        connection_host_stream: &StreamPath,
        review_output_stream: Stream,
        review_id: ReviewId,
        payload: ReviewActionPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_action";
        tracing::info!(
            review_id = %review_id,
            action_kind = payload.kind_name(),
            connection_stream = %connection_host_stream,
            output_stream = %review_output_stream.path(),
            "received review_action"
        );
        let review_registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        review_registry
            .action(
                review_id,
                payload,
                connection_host_stream.clone(),
                review_output_stream,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        Ok(())
    }

    pub(crate) async fn review_subscribe(
        &self,
        connection_host_stream: &StreamPath,
        review_output_stream: Stream,
        review_id: ReviewId,
        include_diffs: bool,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_subscribe";
        tracing::info!(
            review_id = %review_id,
            connection_stream = %connection_host_stream,
            output_stream = %review_output_stream.path(),
            include_diffs,
            "received review_subscribe"
        );
        let review_registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        review_registry
            .subscribe(
                review_id,
                connection_host_stream.clone(),
                review_output_stream,
                include_diffs,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        Ok(())
    }

    async fn deliver_review_payload(
        &self,
        review_id: ReviewId,
        project_id: ProjectId,
        target: ReviewSubmitTarget,
        payload: SendMessagePayload,
    ) -> ReviewDeliveryOutcome {
        let message_len = payload.message.len();
        let images_count = payload.images.as_ref().map_or(0, Vec::len);
        tracing::debug!(
            review_id = %review_id,
            project_id = %project_id,
            target = ?target,
            message_len,
            images_count,
            "resolving review delivery"
        );
        let Some(protocol::MessageOrigin::Review {
            review_id: origin_review_id,
        }) = payload.origin.as_ref()
        else {
            tracing::warn!(
                review_id = %review_id,
                project_id = %project_id,
                reason = "missing_origin",
                "review delivery failed before target resolution"
            );
            return ReviewDeliveryOutcome::Failed(
                "review delivery payload did not carry review origin".to_owned(),
            );
        };
        if origin_review_id != &review_id {
            tracing::warn!(
                review_id = %review_id,
                origin_review_id = %origin_review_id,
                project_id = %project_id,
                reason = "origin_mismatch",
                "review delivery failed before target resolution"
            );
            return ReviewDeliveryOutcome::Failed(format!(
                "review delivery payload origin {} did not match {}",
                origin_review_id, review_id
            ));
        }

        match target {
            ReviewSubmitTarget::ExistingAgent { agent_id } => {
                let target_agent = {
                    let state = self.state.lock().await;
                    let Some(handle) = state.registry.agent_handle(&agent_id) else {
                        tracing::info!(
                            review_id = %review_id,
                            project_id = %project_id,
                            target_agent_id = %agent_id,
                            outcome = "offline",
                            "review delivery target is offline"
                        );
                        return ReviewDeliveryOutcome::Offline;
                    };
                    let start = handle.snapshot();
                    if start.project_id.as_ref() != Some(&project_id) {
                        tracing::warn!(
                            review_id = %review_id,
                            project_id = %project_id,
                            target_agent_id = %agent_id,
                            target_project_id = ?start.project_id,
                            "review delivery target is not in the review project"
                        );
                        return ReviewDeliveryOutcome::Failed(format!(
                            "agent {} is not bound to project {}",
                            agent_id, project_id
                        ));
                    }
                    handle
                };
                if target_agent
                    .send_input(protocol::AgentInput::SendMessage(payload))
                    .await
                {
                    tracing::info!(
                        review_id = %review_id,
                        project_id = %project_id,
                        target_agent_id = %agent_id,
                        message_len,
                        images_count,
                        "delivered review feedback bundle to live agent"
                    );
                    ReviewDeliveryOutcome::Delivered {
                        target_agent_id: agent_id,
                    }
                } else {
                    tracing::warn!(
                        review_id = %review_id,
                        project_id = %project_id,
                        target_agent_id = %agent_id,
                        outcome = "offline",
                        "review delivery target went offline"
                    );
                    ReviewDeliveryOutcome::Offline
                }
            }
            ReviewSubmitTarget::NewAgent {
                backend_kind,
                cost_hint,
                custom_agent_id,
                name,
                instructions,
            } => match self
                .spawn_review_target_agent(ReviewTargetAgentRequest {
                    review_id: review_id.clone(),
                    project_id: project_id.clone(),
                    backend_kind,
                    cost_hint,
                    custom_agent_id,
                    name,
                    instructions,
                    payload,
                })
                .await
            {
                Ok(agent_id) => ReviewDeliveryOutcome::Delivered {
                    target_agent_id: agent_id,
                },
                Err(message) => ReviewDeliveryOutcome::Failed(message),
            },
        }
    }

    async fn spawn_review_target_agent(
        &self,
        request: ReviewTargetAgentRequest,
    ) -> Result<AgentId, String> {
        let ReviewTargetAgentRequest {
            review_id,
            project_id,
            backend_kind,
            cost_hint,
            custom_agent_id,
            name,
            instructions,
            mut payload,
        } = request;
        let (
            project,
            settings_store,
            custom_agent_store,
            mcp_server_store,
            steering_store,
            skill_store,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp,
            config_mcp,
        ) = {
            let state = self.state.lock().await;
            let project = state
                .project_store
                .lock()
                .await
                .get(&project_id)
                .ok_or_else(|| {
                    format!("cannot spawn review target in missing project {project_id}")
                })?;
            (
                project,
                Arc::clone(&state.settings_store),
                Arc::clone(&state.custom_agent_store),
                Arc::clone(&state.mcp_server_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.skill_store),
                state.use_mock_backend,
                state.debug_mcp.clone(),
                state.agent_control_mcp.clone(),
                state.config_mcp.clone(),
            )
        };
        let project_roots = project
            .root_paths()
            .into_iter()
            .map(|root| root.0)
            .collect::<Vec<_>>();
        if project_roots.is_empty() {
            return Err(format!(
                "project {} has no roots for review target",
                project_id
            ));
        }

        let host_settings =
            settings_store.lock().await.get().map_err(|error| {
                format!("failed to load host settings for review target: {error}")
            })?;
        let startup_mcp_servers = startup_mcp_servers_for_settings(
            &host_settings,
            &project_roots,
            &debug_mcp,
            &agent_control_mcp,
            &config_mcp,
            None,
        );
        let mut resolved_spawn_config = {
            let custom_agents = custom_agent_store.lock().await;
            let mcp_servers = mcp_server_store.lock().await;
            let steering = steering_store.lock().await;
            let skills = skill_store.lock().await;
            resolve_spawn_config(ResolveSpawnConfigRequest {
                backend_kind,
                project_id: Some(&project_id),
                custom_agent_id: custom_agent_id.as_ref(),
                built_in_mcp_servers: &startup_mcp_servers,
                custom_agent_store: &custom_agents,
                mcp_server_store: &mcp_servers,
                steering_store: &steering,
                skill_store: &skills,
            })
            .map_err(|error| format!("failed to resolve review target agent config: {error}"))?
        };
        resolved_spawn_config.access_mode = protocol::BackendAccessMode::Unrestricted;
        let startup_mcp_servers =
            protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
        let session_settings_schema = {
            let state = self.state.lock().await;
            session_schema_for_backend(&state, backend_kind)
        };
        let session_settings_schema =
            if backend_kind == protocol::BackendKind::Kiro && session_settings_schema.is_none() {
                self.refresh_session_schemas().await;
                let state = self.state.lock().await;
                session_schema_for_backend(&state, backend_kind)
            } else {
                session_settings_schema
            };
        if let Some(instructions) = instructions
            && !instructions.trim().is_empty()
        {
            payload.message = format!(
                "Instructions for this new review target agent:\n{}\n\n{}",
                instructions.trim(),
                payload.message
            );
        }
        let name = name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Review Feedback".to_owned());
        let request = ResolvedSpawnRequest {
            name: name.clone(),
            origin: protocol::AgentOrigin::AgentControl,
            custom_agent_id,
            team_id: None,
            team_member_id: None,
            parent_agent_id: None,
            parent_session_id: None,
            project_id: Some(project_id.clone()),
            backend_kind,
            workspace_roots: project_roots,
            initial_input: Some(payload),
            cost_hint,
            session_settings: None,
            session_settings_schema,
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id: None,
            fork_from_session_id: None,
            startup_warning: None,
            startup_failure: None,
            initial_alias: Some(InitialAgentAlias {
                name,
                persistence: InitialAgentAliasPersistence::User,
            }),
            use_mock_backend,
        };
        let agent_id = self.spawn_resolved_agent(request).await;
        tracing::info!(
            review_id = %review_id,
            project_id = %project_id,
            target_agent_id = %agent_id,
            backend_kind = ?backend_kind,
            "spawned review target agent"
        );
        Ok(agent_id)
    }

    async fn deliver_submitted_reviews_for_session(&self, session_id: SessionId) {
        tracing::debug!(
            session_id = %session_id,
            "legacy submitted review redelivery is disabled for inline reviews"
        );
    }

    async fn emit_review_list_changed(&self, project_id: ProjectId) {
        let (registry, handle) = {
            let mut state = self.state.lock().await;
            let registry = state.review_registry.clone();
            let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %error,
                        "failed to ensure project actor for review list update"
                    );
                    return;
                }
            };
            (registry, handle)
        };
        let summaries = match registry.summaries(project_id.clone()).await {
            Ok(summaries) => summaries,
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to build review summaries"
                );
                return;
            }
        };
        if let Err(error) = handle
            .emit_project_event(protocol::ProjectEventPayload::ReviewListChanged {
                reviews: summaries,
            })
            .await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to emit review list update"
            );
        }
    }

    async fn spawn_ai_reviewer(
        &self,
        request: ReviewAiSpawnRequest,
    ) -> (
        oneshot::Sender<Result<AgentId, String>>,
        Result<AgentId, String>,
    ) {
        let reply = request.reply;
        let requested_backend_kind = request.backend_kind;
        let backend_kind = match self
            .resolve_ai_reviewer_backend_kind(requested_backend_kind)
            .await
        {
            Ok(backend_kind) => backend_kind,
            Err(message) => return (reply, Err(message)),
        };
        let instructions_len = request.instructions.as_ref().map_or(0, String::len);
        let roots = {
            let state = self.state.lock().await;
            let project = match state
                .project_store
                .lock()
                .await
                .get(&request.review.project_id)
            {
                Some(project) => project,
                None => {
                    return (
                        reply,
                        Err(format!(
                            "cannot spawn AI reviewer for missing project {}",
                            request.review.project_id
                        )),
                    );
                }
            };
            project
                .root_paths()
                .into_iter()
                .map(|root| root.0)
                .collect::<Vec<_>>()
        };
        let roots_count = roots.len();
        let stats = host_diff_stats(&request.review.diffs);
        tracing::info!(
            review_id = %request.review_id,
            backend_kind = ?backend_kind,
            requested_backend_kind = ?requested_backend_kind,
            roots_count,
            diff_count = stats.diff_count,
            file_count = stats.file_count,
            hunk_count = stats.hunk_count,
            line_count = stats.line_count,
            instructions_len,
            "spawning AI reviewer"
        );
        if roots_count == 0 {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                "AI reviewer spawn rejected without diff roots"
            );
            return (reply, Err("review has no frozen diff roots".to_owned()));
        }
        if stats.file_count == 0 {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                roots_count,
                "AI reviewer spawn rejected without changed files"
            );
            return (
                reply,
                Err("review has no changed files to review".to_owned()),
            );
        }
        let review_mcp_url = {
            let state = self.state.lock().await;
            state.review_mcp.url.clone()
        };
        if review_mcp_url.trim().is_empty() {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                "AI reviewer spawn rejected without review MCP URL"
            );
            return (
                reply,
                Err("review feedback MCP server is unavailable for AI review".to_owned()),
            );
        }
        let reviewer_system_prompt =
            build_reviewer_system_prompt(&request.review, request.instructions);
        let reviewer_system_prompt_len = reviewer_system_prompt.len();
        let reviewer_spawn_config = ResolvedSpawnConfig {
            instructions: Some(reviewer_system_prompt),
            steering_body: String::new(),
            skills: Vec::new(),
            mcp_servers: vec![McpServerConfig {
                id: McpServerId("tyde-review-feedback".to_owned()),
                name: REVIEW_FEEDBACK_MCP_SERVER_NAME.to_owned(),
                transport: McpTransportConfig::Http {
                    url: review_mcp_url,
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }],
            tool_policy: reviewer_tool_policy(),
            access_mode: protocol::BackendAccessMode::ReadOnly,
        };
        let prompt = build_reviewer_user_prompt();
        let prompt_len = prompt.len();
        let payload = SpawnAgentPayload {
            name: Some("AI Review".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(request.review.project_id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: roots,
                prompt,
                images: None,
                backend_kind,
                cost_hint: request.cost_hint,
                access_mode: protocol::BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        };
        tracing::debug!(
            review_id = %request.review_id,
            backend_kind = ?backend_kind,
            roots_count,
            reviewer_system_prompt_len,
            prompt_len,
            "dispatching AI reviewer spawn"
        );
        let agent_id = self
            .spawn_agent_with_origin_and_config(
                payload,
                protocol::AgentOrigin::AgentControl,
                Some(reviewer_spawn_config),
            )
            .await;
        if let Some(agent_handle) = self.agent_handle(&agent_id).await {
            tracing::info!(
                review_id = %request.review_id,
                reviewer_agent_id = %agent_id,
                "AI reviewer spawned; attaching tool bridge"
            );
            ReviewerToolBridge::spawn(agent_id.clone(), agent_handle, request.review_handle);
            (reply, Ok(agent_id))
        } else {
            tracing::warn!(
                review_id = %request.review_id,
                reviewer_agent_id = %agent_id,
                "AI reviewer spawned but tool bridge attach target was missing"
            );
            (
                reply,
                Err(format!(
                    "spawned AI reviewer {} but could not attach tool bridge",
                    agent_id
                )),
            )
        }
    }

    async fn resolve_ai_reviewer_backend_kind(
        &self,
        requested: Option<protocol::BackendKind>,
    ) -> Result<protocol::BackendKind, String> {
        if let Some(backend_kind) = requested {
            return Ok(backend_kind);
        }
        let settings_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.settings_store)
        };
        let settings = settings_store
            .lock()
            .await
            .get()
            .map_err(|error| format!("failed to load host settings for AI review: {error}"))?;
        settings
            .default_backend
            .or_else(|| settings.enabled_backends.first().copied())
            .ok_or_else(|| {
                "backend_kind is required because the host has no default_backend or enabled backends"
                    .to_owned()
            })
    }
}

async fn load_project(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    operation: &'static str,
) -> AppResult<Project> {
    let projects = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(operation, error))?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| AppError::not_found(operation, format!("project {} not found", project_id)))
}

fn read_review_diffs(
    project: &Project,
    selection: &ReviewDiffSelection,
) -> Result<Vec<protocol::ProjectGitDiffPayload>, String> {
    match selection {
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => {
            let mut diffs = Vec::new();
            for root in project.root_paths() {
                let payload = ProjectReadDiffPayload {
                    root,
                    scope: protocol::ProjectDiffScope::Unstaged,
                    path: None,
                    context_mode: protocol::DiffContextMode::FullFile,
                };
                match read_diff(project, payload) {
                    Ok(diff) => diffs.push(diff),
                    Err(error) if is_not_git_repository_error(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(diffs)
        }
        ReviewDiffSelection::Root { root, path, .. } => {
            let payload = ProjectReadDiffPayload {
                root: root.clone(),
                scope: protocol::ProjectDiffScope::Unstaged,
                path: path.clone(),
                context_mode: protocol::DiffContextMode::FullFile,
            };
            match read_diff(project, payload) {
                Ok(diff) => Ok(vec![diff]),
                Err(error) if is_not_git_repository_error(&error) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HostDiffStats {
    diff_count: usize,
    file_count: usize,
    hunk_count: usize,
    line_count: usize,
}

fn host_diff_stats(diffs: &[protocol::ProjectGitDiffPayload]) -> HostDiffStats {
    let file_count = diffs.iter().map(|diff| diff.files.len()).sum();
    let hunk_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .map(|file| file.hunks.len())
        .sum();
    let line_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .flat_map(|file| file.hunks.iter())
        .map(|hunk| hunk.lines.len())
        .sum();
    HostDiffStats {
        diff_count: diffs.len(),
        file_count,
        hunk_count,
        line_count,
    }
}

fn compute_workbench_roots(
    parent_roots: &[ProjectRootPath],
    branch: &GitBranchName,
) -> AppResult<Vec<WorkbenchRoot>> {
    parent_roots
        .iter()
        .map(|parent_root| {
            Ok(WorkbenchRoot {
                parent_root: parent_root.clone(),
                worktree_root: compute_worktree_path(parent_root, branch)
                    .map_err(|error| AppError::invalid("workbench_create", error))?,
            })
        })
        .collect()
}

fn compute_worktree_path(
    parent_root: &ProjectRootPath,
    branch: &GitBranchName,
) -> Result<ProjectRootPath, String> {
    let parent_path = Path::new(&parent_root.0);
    let basename = parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("parent root {} has no UTF-8 basename", parent_root))?;
    let sibling_name = format!("{}--{}", basename, percent_encode_branch(&branch.0));
    let worktree_path = match parent_path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => PathBuf::from(sibling_name),
        Some(parent) => parent.join(sibling_name),
        None => PathBuf::from(sibling_name),
    };
    let worktree_path = worktree_path.to_str().ok_or_else(|| {
        format!(
            "computed worktree path for parent root {} is not UTF-8",
            parent_root
        )
    })?;
    Ok(ProjectRootPath(worktree_path.to_owned()))
}

fn percent_encode_branch(branch: &str) -> String {
    let mut encoded = String::with_capacity(branch.len());
    for byte in branch.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                encoded.push(*byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(hex_digit(byte >> 4));
                encoded.push(hex_digit(byte & 0x0f));
            }
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + (value - 10)) as char,
        _ => unreachable!("hex_digit value must be in 0..=15"),
    }
}

async fn preflight_workbench_create(
    project_store: &Arc<Mutex<ProjectStore>>,
    parent: &Project,
    branch: &GitBranchName,
    roots: &[WorkbenchRoot],
) -> AppResult<()> {
    const OPERATION: &str = "workbench_create";
    if !matches!(parent.source, ProjectSource::Standalone { .. }) {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "cannot create workbench for non-standalone parent project {}",
                parent.id
            ),
        ));
    }

    for root in roots {
        ensure_git_top_level(&root.parent_root).await?;
    }
    for root in roots {
        let output = run_git(
            &root.parent_root,
            &["check-ref-format", "--branch", &branch.0],
        )
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
        if !output.status.success() {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "invalid git branch {} for parent root {}: {}",
                    branch,
                    root.parent_root,
                    git_output_message(&output)
                ),
            ));
        }
    }
    for root in roots {
        if git_branch_exists(&root.parent_root, branch).await? {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "branch {} already exists in parent root {}",
                    branch, root.parent_root
                ),
            ));
        }
    }
    for root in roots {
        let exists = tokio::fs::try_exists(&root.worktree_root.0)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        if exists {
            return Err(AppError::conflict(
                OPERATION,
                format!("worktree path {} already exists", root.worktree_root),
            ));
        }
    }

    let records = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(OPERATION, error))?;
    for root in roots {
        if let Some(owner) = records.iter().find(|project| {
            project
                .root_paths()
                .into_iter()
                .any(|candidate| candidate == root.worktree_root)
        }) {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "worktree path {} is already registered as project root for project {}",
                    root.worktree_root, owner.id
                ),
            ));
        }
    }

    Ok(())
}

async fn ensure_git_top_level(parent_root: &ProjectRootPath) -> AppResult<()> {
    const OPERATION: &str = "workbench_create";
    let output = run_git(parent_root, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
    if !output.status.success() {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "parent root {} is not a git top-level: {}",
                parent_root,
                git_output_message(&output)
            ),
        ));
    }
    let top_level = String::from_utf8(output.stdout).map_err(|error| {
        AppError::internal_message(
            OPERATION,
            format!(
                "git rev-parse --show-toplevel for {} returned non-UTF-8 output",
                parent_root
            ),
            error,
        )
    })?;
    let top_level = top_level.trim();
    let root_canonical = tokio::fs::canonicalize(&parent_root.0)
        .await
        .map_err(|error| {
            AppError::invalid(
                OPERATION,
                format!("parent root {} is invalid: {}", parent_root, error),
            )
        })?;
    let top_level_canonical = tokio::fs::canonicalize(top_level)
        .await
        .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
    if root_canonical != top_level_canonical {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "parent root {} is not a git top-level; git top-level is {}",
                parent_root, top_level
            ),
        ));
    }
    Ok(())
}

async fn git_branch_exists(
    parent_root: &ProjectRootPath,
    branch: &GitBranchName,
) -> AppResult<bool> {
    let ref_name = format!("refs/heads/{}", branch.0);
    let output = run_git(
        parent_root,
        &["rev-parse", "--verify", "--quiet", &ref_name],
    )
    .await
    .map_err(|error| {
        AppError::internal_message("workbench_create", error.clone(), anyhow!(error))
    })?;
    Ok(output.status.success())
}

async fn git_worktree_add(
    parent_root: &ProjectRootPath,
    worktree_root: &ProjectRootPath,
    branch: &GitBranchName,
) -> Result<(), String> {
    let output = run_git(
        parent_root,
        &["worktree", "add", "-b", &branch.0, &worktree_root.0],
    )
    .await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree add failed for parent root {} branch {} worktree {}: {}",
        parent_root,
        branch,
        worktree_root,
        git_output_message(&output)
    ))
}

async fn git_worktree_remove(
    parent_root: &ProjectRootPath,
    worktree_root: &ProjectRootPath,
) -> Result<(), String> {
    let output = run_git(parent_root, &["worktree", "remove", &worktree_root.0]).await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree remove failed for parent root {} worktree {}: {}",
        parent_root,
        worktree_root,
        git_output_message(&output)
    ))
}

async fn git_status_porcelain(worktree_root: &ProjectRootPath) -> Result<String, String> {
    let output = run_git(
        worktree_root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "git status failed for worktree root {}: {}",
            worktree_root,
            git_output_message(&output)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git status output was not valid UTF-8: {error}"))
}

async fn rollback_created_worktrees(
    created: &[WorkbenchRoot],
    branch: &GitBranchName,
) -> Option<String> {
    let mut failures = Vec::new();
    for root in created.iter().rev() {
        match run_git(
            &root.parent_root,
            &["worktree", "remove", "--force", &root.worktree_root.0],
        )
        .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "rollback git worktree remove --force failed for {}: {}",
                root.worktree_root,
                git_output_message(&output)
            )),
            Err(error) => failures.push(format!(
                "rollback git worktree remove --force failed for {}: {}",
                root.worktree_root, error
            )),
        }
        // `git worktree add -b` created this branch in the parent repo;
        // delete it so retrying the identical create passes the
        // branch-exists preflight. The branch still points at the parent
        // HEAD (no work happened), so `-D` cannot lose anything.
        match run_git(&root.parent_root, &["branch", "-D", &branch.0]).await {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "rollback git branch -D {} failed for {}: {}",
                branch,
                root.parent_root,
                git_output_message(&output)
            )),
            Err(error) => failures.push(format!(
                "rollback git branch -D {} failed for {}: {}",
                branch, root.parent_root, error
            )),
        }
    }
    if failures.is_empty() {
        None
    } else {
        Some(failures.join("; "))
    }
}

async fn git_worktree_prune(parent_root: &ProjectRootPath) -> Result<(), String> {
    let output = run_git(parent_root, &["worktree", "prune"]).await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree prune failed for parent root {}: {}",
        parent_root,
        git_output_message(&output)
    ))
}

async fn cleanup_reviews_for_deleted_project(
    review_registry: &ReviewRegistryHandle,
    project_id: &ProjectId,
) {
    match review_registry.delete_for_project(project_id.clone()).await {
        Ok(removed) if removed.is_empty() => {}
        Ok(removed) => tracing::info!(
            project_id = %project_id,
            review_count = removed.len(),
            review_ids = ?removed,
            "deleted persisted reviews referencing removed project"
        ),
        Err(error) => tracing::warn!(
            project_id = %project_id,
            error = %error,
            "failed to delete persisted reviews referencing removed project"
        ),
    }
}

fn append_rollback_message(error: String, rollback_message: Option<String>) -> String {
    match rollback_message {
        Some(rollback_message) => format!("{error}; rollback failures: {rollback_message}"),
        None => error,
    }
}

async fn run_git(root: &ProjectRootPath, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(&root.0)
        .args(args)
        .output()
        .await
        .map_err(|error| {
            format!(
                "failed to run git -C {} {}: {}",
                root,
                args.join(" "),
                error
            )
        })
}

fn git_output_message(output: &std::process::Output) -> String {
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_owned());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "exit code {}; stdout: {}; stderr: {}",
        code,
        stdout.trim(),
        stderr.trim()
    )
}

async fn project_exists(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    operation: &'static str,
) -> AppResult<bool> {
    let projects = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(operation, error))?;
    Ok(projects
        .into_iter()
        .any(|project| project.id == *project_id))
}

fn project_store_error(operation: &'static str, error: ProjectStoreError) -> AppError {
    match error {
        ProjectStoreError::NotFound(message) => AppError::not_found(operation, message),
        ProjectStoreError::InvalidInput(message) => AppError::invalid(operation, message),
        ProjectStoreError::Conflict(message) => AppError::conflict(operation, message),
        ProjectStoreError::InvalidStore(message) | ProjectStoreError::Internal(message) => {
            AppError::internal_message(operation, message.clone(), anyhow!(message))
        }
    }
}

fn custom_agent_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing custom agent") {
        AppError::not_found(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid custom agent store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn steering_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing steering") {
        AppError::not_found(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid steering store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn skill_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing skill") {
        AppError::not_found(operation, error)
    } else if error.contains("duplicate directory name") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid skills index") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn mcp_server_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing MCP server") {
        AppError::not_found(operation, error)
    } else if error.contains("duplicate name") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid MCP server store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn referenced_team_member_delete_message(
    resource_kind: &str,
    resource_id: &impl std::fmt::Display,
    resource_name: Option<&str>,
    snapshot: &TeamRegistrySnapshot,
    referenced_member_ids: &[TeamMemberId],
) -> String {
    let resource = match resource_name {
        Some(name) => format!(r#"{resource_kind} "{name}""#),
        None => format!("{resource_kind} {resource_id}"),
    };
    let Some(first_member_id) = referenced_member_ids.first() else {
        return format!("cannot delete {resource} while referenced by a team member");
    };
    let message = if let Some(member) = snapshot
        .members
        .iter()
        .find(|member| member.id == *first_member_id)
    {
        if let Some(team) = snapshot.teams.iter().find(|team| team.id == member.team_id) {
            format!(
                r#"cannot delete {resource} while referenced by team member "{}" in team "{}""#,
                member.name, team.name
            )
        } else {
            format!(
                r#"cannot delete {resource} while referenced by team member "{}" in team {}"#,
                member.name, member.team_id
            )
        }
    } else {
        format!("cannot delete {resource} while referenced by team member {first_member_id}")
    };
    let remaining = referenced_member_ids.len().saturating_sub(1);
    if remaining > 0 {
        format!("{message} (and {remaining} more)")
    } else {
        message
    }
}

fn team_member_activation_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("conflict:") || error.contains("activation is already in progress") {
        AppError::conflict(operation, error)
    } else if error.starts_with("failed to load sessions")
        || error.starts_with("Failed ")
        || error.starts_with("Invalid agent teams store")
        || error.contains("references missing team")
        || error.contains("bound to missing agent")
    {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else if error.starts_with("cannot resume") || error.contains("agent backend is closed") {
        AppError::conflict(operation, error)
    } else {
        AppError::invalid(operation, error)
    }
}

fn team_registry_error(operation: &'static str, error: String) -> AppError {
    if error.contains("references missing custom agent")
        || error.contains("references missing project")
    {
        AppError::conflict(operation, error)
    } else if error.contains("missing") {
        AppError::not_found(operation, error)
    } else if error.contains("already")
        || error.contains("active manager")
        || error.contains("live-bound")
        || error.starts_with("conflict:")
    {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid agent teams store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn session_store_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("Failed ") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn project_command_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("No unstaged diff exists") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed to run git")
        || error.starts_with("git ")
        || error.starts_with("git output was not valid UTF-8")
        || error.starts_with("Failed to read untracked file")
        || error.starts_with("invalid diff header")
        || error.starts_with("invalid hunk")
        || error.starts_with("missing old range in hunk header")
        || error.starts_with("missing new range in hunk header")
        || error.contains("appeared before file in git diff")
    {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

pub fn spawn_host() -> HostHandle {
    let session_path = SessionStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default session store path: {err}"));
    let project_path = ProjectStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default project store path: {err}"));
    let agent_team_path = AgentTeamsStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default agent teams store path: {err}"));
    let review_path = ReviewStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default review store path: {err}"));
    let settings_path = HostSettingsStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default settings store path: {err}"));
    let custom_agent_path = CustomAgentStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default custom agent store path: {err}"));
    let mcp_server_path = McpServerStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default MCP server store path: {err}"));
    let steering_path = SteeringStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default steering store path: {err}"));
    let skills_index_path = SkillStore::default_index_path()
        .unwrap_or_else(|err| panic!("failed to resolve default skills index path: {err}"));
    let skills_root_dir = SkillStore::default_root_dir()
        .unwrap_or_else(|err| panic!("failed to resolve default skills root dir: {err}"));
    let mobile_pairings_path = MobilePairingsStore::default_path().unwrap_or_else(|err| {
        panic!("failed to resolve default mobile pairings store path: {err}")
    });
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: agent_team_path,
            review: review_path,
            settings: settings_path,
            custom_agent: custom_agent_path,
            mcp_server: mcp_server_path,
            steering: steering_path,
            skills_index: skills_index_path,
            skills_root_dir,
            mobile_pairings: mobile_pairings_path,
        },
        false,
        HostRuntimeConfig::default(),
    )
    .unwrap_or_else(|err| panic!("failed to initialize host stores: {err}"))
}

pub fn spawn_host_with_session_store(path: PathBuf) -> Result<HostHandle, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Session store path has no parent: {}", path.display()))?;
    let project_path = parent.join("projects.json");
    let settings_path = parent.join("settings.json");
    spawn_host_with_store_paths_and_runtime_config(
        path,
        project_path,
        settings_path,
        HostRuntimeConfig::default(),
    )
}

pub fn spawn_host_with_store_paths(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    spawn_host_with_store_paths_and_runtime_config(
        session_path,
        project_path,
        settings_path,
        HostRuntimeConfig::default(),
    )
}

pub fn spawn_host_with_store_paths_and_runtime_config(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let parent = session_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| {
            format!(
                "Session store path has no parent: {}",
                session_path.display()
            )
        })?;
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: parent.join("agent_teams.json"),
            review: parent.join("reviews.json"),
            settings: settings_path,
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
            mobile_pairings: MobilePairingsStore::path_for_store_parent(&parent),
        },
        false,
        runtime_config,
    )
}

/// Spawn a host that uses MockBackend for all agent spawns (for tests).
pub fn spawn_host_with_mock_backend(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    spawn_host_with_mock_backend_and_runtime_config(
        session_path,
        project_path,
        settings_path,
        HostRuntimeConfig::default(),
    )
}

pub fn spawn_host_with_mock_backend_and_runtime_config(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let parent = session_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| {
            format!(
                "Session store path has no parent: {}",
                session_path.display()
            )
        })?;
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: parent.join("agent_teams.json"),
            review: parent.join("reviews.json"),
            settings: settings_path,
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
            mobile_pairings: MobilePairingsStore::path_for_store_parent(&parent),
        },
        true,
        runtime_config,
    )
}

fn spawn_host_inner(
    paths: HostStorePaths,
    use_mock_backend: bool,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let (session_store, purged_gemini_session_ids) =
        SessionStore::load_with_migration(paths.session)?;
    let project_store = ProjectStore::load(paths.project)?;
    let review_store = ReviewStore::load(paths.review)?;
    let settings_store = HostSettingsStore::load(paths.settings)?;
    let host_settings = settings_store.get()?;
    let initial_mobile_settings = host_settings.clone();
    let mobile_pairings_store = MobilePairingsStore::load(paths.mobile_pairings)?;
    let custom_agent_store = CustomAgentStore::load(paths.custom_agent)?;
    let (role_preset_ids, personality_preset_ids) = team_preset_validation_refs();
    let team_refs = AgentTeamValidationRefs {
        custom_agent_ids: custom_agent_store
            .list()?
            .into_iter()
            .map(|custom_agent| custom_agent.id)
            .collect(),
        project_ids: project_store
            .list()?
            .into_iter()
            .map(|project| project.id)
            .collect(),
        enabled_backend_kinds: host_settings.enabled_backends.iter().copied().collect(),
        role_preset_ids,
        personality_preset_ids,
        legacy_backend_kind: host_settings.default_backend,
        purged_gemini_session_ids,
    };
    let team_store = AgentTeamsStore::load(paths.agent_team, &team_refs)?;
    let project_store = Arc::new(Mutex::new(project_store));
    let mcp_server_store = McpServerStore::load(paths.mcp_server)?;
    let steering_store = SteeringStore::load(paths.steering)?;
    let skill_store = SkillStore::load(paths.skills_index, paths.skills_root_dir)?;
    let (sub_agent_spawn_tx, sub_agent_spawn_rx) =
        mpsc::unbounded_channel::<HostSubAgentSpawnRequest>();
    let (mobile_access_tx, mobile_access_rx) = mpsc::unbounded_channel::<MobileAccessCommand>();
    let mobile_access = MobileAccessHandle::new(mobile_access_tx.clone());
    let (review_delivery_tx, review_delivery_rx) = mpsc::channel::<ReviewDeliveryRequest>(64);
    let (review_ai_spawn_tx, review_ai_spawn_rx) = mpsc::channel::<ReviewAiSpawnRequest>(16);
    let (review_project_update_tx, review_project_update_rx) =
        mpsc::unbounded_channel::<ProjectId>();
    let review_registry = ReviewRegistry::spawn(
        review_store,
        Arc::clone(&project_store),
        review_delivery_tx,
        review_ai_spawn_tx,
        review_project_update_tx,
    )?;
    let debug_mcp = match crate::debug_mcp::start_server(runtime_config.debug_mcp_bind_addr) {
        Ok(handle) => handle,
        Err(err) if runtime_config.debug_mcp_bind_addr.is_none() => {
            tracing::warn!(
                "debug MCP server unavailable; continuing without it: {}",
                err
            );
            crate::debug_mcp::DebugMcpHandle { url: String::new() }
        }
        Err(err) => return Err(err),
    };

    // Create the host handle first so we can pass it to the agent-control MCP
    // server. The MCP server runs on its own thread and accesses the host
    // through the cloned handle.
    let agent_control_mcp_placeholder = AgentControlMcpHandle { url: String::new() };
    let config_mcp_placeholder = ConfigMcpHandle { url: String::new() };
    let review_mcp_placeholder = ReviewMcpHandle { url: String::new() };
    let host = HostHandle {
        state: Arc::new(Mutex::new(HostState {
            registry: AgentRegistry::new(),
            review_registry,
            team_registry: TeamRegistryHandle::spawn(team_store),
            project_store,
            settings_store: Arc::new(Mutex::new(settings_store)),
            session_store: Arc::new(Mutex::new(session_store)),
            custom_agent_store: Arc::new(Mutex::new(custom_agent_store)),
            mcp_server_store: Arc::new(Mutex::new(mcp_server_store)),
            steering_store: Arc::new(Mutex::new(steering_store)),
            skill_store: Arc::new(Mutex::new(skill_store)),
            agent_sessions: HashMap::new(),
            sub_agent_spawn_tx,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp: agent_control_mcp_placeholder,
            config_mcp: config_mcp_placeholder,
            review_mcp: review_mcp_placeholder,
            mobile_access: mobile_access.clone(),
            kiro_session_schema: KiroSessionSchemaState::Pending,
            kiro_probe_program: runtime_config.kiro_probe_program.clone(),
            host_streams: HashMap::new(),
            project_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
            browse_streams: HashMap::new(),
            workbench_parent_locks: HashMap::new(),
            project_search_ids: HashMap::new(),
            removing_projects: HashSet::new(),
        })),
    };

    spawn_mobile_access_actor(
        host.clone(),
        mobile_access_tx,
        mobile_access_rx,
        MobileAccessInit {
            pairings_store: mobile_pairings_store,
            initial_settings: initial_mobile_settings,
            pairing_ttl: runtime_config
                .mobile_pairing_ttl
                .unwrap_or(crate::mobile_access::DEFAULT_PAIRING_TTL),
        },
    )?;

    let agent_control_mcp = match crate::agent_control_mcp::start_server(
        runtime_config.agent_control_mcp_bind_addr,
        host.clone(),
    ) {
        Ok(handle) => handle,
        Err(err) if runtime_config.agent_control_mcp_bind_addr.is_none() => {
            tracing::warn!(
                "agent-control MCP server unavailable; continuing without it: {}",
                err
            );
            AgentControlMcpHandle { url: String::new() }
        }
        Err(err) => return Err(err),
    };

    // Store the real handle now that the server is running. We just created
    // the Arc<Mutex<HostState>> above and no async tasks are holding it yet,
    // so try_lock always succeeds.
    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .agent_control_mcp = agent_control_mcp;

    let config_mcp = match crate::config_mcp::start_server(None, host.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(
                "config MCP server unavailable; continuing without it: {}",
                err
            );
            ConfigMcpHandle { url: String::new() }
        }
    };
    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .config_mcp = config_mcp;

    let review_mcp =
        match crate::review_mcp::start_server(runtime_config.review_mcp_bind_addr, host.clone()) {
            Ok(handle) => handle,
            Err(err) if runtime_config.review_mcp_bind_addr.is_none() => {
                tracing::warn!(
                    "review MCP server unavailable; continuing without it: {}",
                    err
                );
                ReviewMcpHandle { url: String::new() }
            }
            Err(err) => return Err(err),
        };

    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .review_mcp = review_mcp;

    spawn_host_sub_agent_task(host.clone(), sub_agent_spawn_rx);
    spawn_host_review_delivery_task(host.clone(), review_delivery_rx);
    spawn_host_review_ai_task(host.clone(), review_ai_spawn_rx);
    spawn_host_review_project_update_task(host.clone(), review_project_update_rx);
    spawn_host_team_status_task(host.clone());

    Ok(host)
}

fn spawn_host_team_status_task(host: HostHandle) {
    let worker = async move {
        let mut status_rx = host.subscribe_agent_status_changes().await;
        let mut last_seen = HashMap::<AgentId, u64>::new();
        loop {
            if status_rx.changed().await.is_err() {
                break;
            }
            let entries = {
                let state = host.state.lock().await;
                state
                    .registry
                    .agent_ids()
                    .into_iter()
                    .filter_map(|agent_id| {
                        state
                            .registry
                            .agent_status_handle(&agent_id)
                            .map(|status_handle| (agent_id, status_handle))
                    })
                    .collect::<Vec<_>>()
            };
            let live_agent_ids = entries
                .iter()
                .map(|(agent_id, _)| agent_id.clone())
                .collect::<HashSet<_>>();
            last_seen.retain(|agent_id, _| live_agent_ids.contains(agent_id));

            for (agent_id, status_handle) in entries {
                let status = status_handle.snapshot().await;
                if last_seen.get(&agent_id).copied() == Some(status.activity_counter) {
                    continue;
                }
                last_seen.insert(agent_id.clone(), status.activity_counter);
                let registry = { host.state.lock().await.team_registry.clone() };
                let result = if status.terminated {
                    registry.clear_binding_by_agent(agent_id.clone()).await
                } else {
                    registry
                        .record_agent_activity(agent_id.clone(), status.status())
                        .await
                };
                match result {
                    Ok(events) => host.fan_out_team_registry_events(events).await,
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %agent_id,
                            error = %error,
                            "failed to update team member binding from agent status"
                        );
                    }
                }
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-host-team-status".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to build host team-status runtime"
                    );
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(
            error = %err,
            "failed to spawn host team-status worker thread"
        );
    }
}

fn spawn_host_sub_agent_task(host: HostHandle, mut rx: HostSubAgentSpawnRx) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let handle = host.spawn_backend_native_subagent(&request).await;
            let _ = request.reply.send(handle);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-host-subagents".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build host sub-agent runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn host sub-agent worker thread");
}

fn spawn_host_review_delivery_task(
    host: HostHandle,
    mut rx: mpsc::Receiver<ReviewDeliveryRequest>,
) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let review_id = request.review_id.clone();
            let project_id = request.project_id.clone();
            let message_len = request.payload.message.len();
            let images_count = request.payload.images.as_ref().map_or(0, Vec::len);
            tracing::debug!(
                review_id = %review_id,
                project_id = %project_id,
                message_len,
                images_count,
                "review delivery worker received request"
            );
            let outcome = host
                .deliver_review_payload(
                    request.review_id,
                    request.project_id,
                    request.target,
                    request.payload,
                )
                .await;
            tracing::debug!(
                review_id = %review_id,
                project_id = %project_id,
                outcome = outcome.label(),
                "review delivery worker completed request"
            );
            let _ = request.reply.send(outcome);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-delivery".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review delivery runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review delivery worker thread");
}

fn spawn_host_review_ai_task(host: HostHandle, mut rx: mpsc::Receiver<ReviewAiSpawnRequest>) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let (reply, result) = host.spawn_ai_reviewer(request).await;
            let _ = reply.send(result);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-ai".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review AI runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review AI worker thread");
}

fn spawn_host_review_project_update_task(
    host: HostHandle,
    mut rx: mpsc::UnboundedReceiver<ProjectId>,
) {
    let worker = async move {
        while let Some(project_id) = rx.recv().await {
            host.emit_review_list_changed(project_id).await;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-projects".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review project update runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review project update worker thread");
}

pub(crate) fn startup_mcp_servers_for_settings(
    settings: &protocol::HostSettings,
    workspace_roots: &[String],
    debug_mcp: &DebugMcpHandle,
    agent_control_mcp: &AgentControlMcpHandle,
    config_mcp: &ConfigMcpHandle,
    custom_agent_id: Option<&protocol::CustomAgentId>,
) -> Vec<StartupMcpServer> {
    let mut servers = Vec::new();

    // The builtin Help agent gets the host-configuration tools; no other
    // agent does.
    if custom_agent_id.is_some_and(|id| id.0 == crate::store::custom_agents::HELP_CUSTOM_AGENT_ID)
        && !config_mcp.url.is_empty()
    {
        servers.push(StartupMcpServer {
            name: "tyde-config".to_string(),
            transport: StartupMcpTransport::Http {
                url: config_mcp.url.clone(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        });
    }

    if settings.tyde_debug_mcp_enabled {
        let mut headers = HashMap::new();
        if let Some(repo_root) = workspace_roots
            .first()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            headers.insert(
                crate::debug_mcp::DEBUG_REPO_ROOT_HEADER.to_string(),
                repo_root.to_string(),
            );
        }
        let url = workspace_roots
            .first()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|repo_root| debug_mcp_url_for_repo_root(&debug_mcp.url, repo_root))
            .unwrap_or_else(|| debug_mcp.url.clone());
        servers.push(StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var: None,
            },
        });
    }

    if settings.tyde_agent_control_mcp_enabled && !agent_control_mcp.url.is_empty() {
        servers.push(StartupMcpServer {
            name: "tyde-agent-control".to_string(),
            transport: StartupMcpTransport::Http {
                url: agent_control_mcp.url.clone(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        });
    }

    servers
}

fn prepend_manager_roster(team: &protocol::Team, members: &[TeamMember], prompt: String) -> String {
    let mut block = String::new();
    block.push_str(
        "You are the manager for this Tyde agent team. Current roster:
",
    );
    block.push_str(&format!(
        "Team: {} ({})
",
        team.name, team.id
    ));
    for member in members {
        if member.role != TeamMemberRole::Report {
            continue;
        }
        block.push_str(
            "
Report:
",
        );
        block.push_str(&format!(
            "- member_id: {}
",
            member.id
        ));
        block.push_str(&format!(
            "- name: {}
",
            member.name
        ));
        block.push_str(&format!(
            "- description: {}
",
            member.description
        ));
        if let Some(profile) = member.profile.as_ref() {
            if let Some(role_preset_id) = profile.role_preset_id.as_ref() {
                block.push_str(&format!("- role_preset_id: {}\n", role_preset_id));
            }
            if let Some(personality_preset_id) = profile.personality_preset_id.as_ref() {
                block.push_str(&format!(
                    "- personality_preset_id: {}\n",
                    personality_preset_id
                ));
            }
            if !profile.personality_traits.is_empty() {
                block.push_str(&format!(
                    "- personality_traits: {:?}\n",
                    profile.personality_traits
                ));
            }
        }
        let project_ids = member
            .project_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        block.push_str(&format!(
            "- project_ids: {:?}
",
            project_ids
        ));
    }
    block.push_str("
Use tyde_team_describe when you need the current roster and tyde_team_message_member to delegate to reports.

User request:
");
    block.push_str(&prompt);
    block
}

fn team_member_primary_project_id(member: &TeamMember) -> Result<ProjectId, String> {
    member
        .project_ids
        .first()
        .cloned()
        .ok_or_else(|| format!("team member {} has no project_ids", member.id))
}

fn debug_mcp_url_for_repo_root(base_url: &str, repo_root: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{separator}repo_root={}",
        percent_encode_query_component(repo_root)
    )
}

pub(crate) fn mcp_url_for_agent(base_url: &str, agent_id: &AgentId) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{separator}agent_id={}",
        percent_encode_query_component(&agent_id.0)
    )
}

fn percent_encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    encoded
}

async fn emit_new_agent_for_stream(
    start: &AgentStartPayload,
    agent_handle: &AgentHandle,
    stream: &Stream,
) -> Result<(), StreamClosed> {
    let instance_stream = new_instance_stream(&start.agent_id);

    let new_agent = NewAgentPayload {
        agent_id: start.agent_id.clone(),
        name: start.name.clone(),
        origin: start.origin,
        backend_kind: start.backend_kind,
        workspace_roots: start.workspace_roots.clone(),
        custom_agent_id: start.custom_agent_id.clone(),
        team_id: start.team_id.clone(),
        team_member_id: start.team_member_id.clone(),
        project_id: start.project_id.clone(),
        parent_agent_id: start.parent_agent_id.clone(),
        session_id: start.session_id.clone(),
        created_at_ms: start.created_at_ms,
        instance_stream: instance_stream.clone(),
    };

    let payload = serde_json::to_value(&new_agent)
        .expect("failed to serialize NewAgent payload for host stream fanout");
    stream.send_value(FrameKind::NewAgent, payload)?;

    let agent_stream = stream.with_path(instance_stream);
    let attached = agent_handle.attach(agent_stream).await;
    assert!(
        attached,
        "failed to attach newly spawned agent stream {}; registry is inconsistent",
        start.agent_id
    );

    Ok(())
}

async fn fan_out_session_lists(state: &mut HostState) {
    let mut sessions = state
        .session_store
        .lock()
        .await
        .summaries()
        .unwrap_or_else(|err| panic!("failed to list sessions for fanout: {err}"));
    normalize_antigravity_session_resumability(&mut sessions);
    let payload = serde_json::to_value(SessionListPayload { sessions })
        .expect("failed to serialize SessionList payload for host stream fanout");

    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if subscriber
            .stream
            .send_value(FrameKind::SessionList, payload.clone())
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

fn normalize_antigravity_session_resumability(sessions: &mut [SessionSummary]) {
    normalize_antigravity_session_resumability_with(
        sessions,
        crate::backend::antigravity::is_antigravity_session_resumable,
    );
}

fn normalize_antigravity_session_resumability_with<F>(
    sessions: &mut [SessionSummary],
    is_antigravity_resumable: F,
) where
    F: Fn(&SessionId) -> bool,
{
    for session in sessions {
        if session.backend_kind == protocol::BackendKind::Antigravity {
            session.resumable = !antigravity_summary_is_permanently_non_resumable(session)
                && is_antigravity_resumable(&session.id);
        }
    }
}

fn antigravity_summary_is_permanently_non_resumable(session: &SessionSummary) -> bool {
    session.compacted_to_session_id.is_some() || (!session.resumable && session.parent_id.is_some())
}

async fn fan_out_project_notify(state: &mut HostState, payload: ProjectNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_project_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_custom_agent_notify(state: &mut HostState, payload: CustomAgentNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_custom_agent_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_steering_notify(state: &mut HostState, payload: SteeringNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_steering_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_skill_notify(state: &mut HostState, payload: SkillNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_skill_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_mcp_server_notify(state: &mut HostState, payload: McpServerNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_mcp_server_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_registry_events(state: &mut HostState, events: TeamRegistryEvents) {
    for payload in events.team_notifies {
        fan_out_team_notify(state, payload).await;
    }
    for payload in events.member_notifies {
        fan_out_team_member_notify(state, payload).await;
    }
    for payload in events.binding_notifies {
        fan_out_team_member_binding_notify(state, payload).await;
    }
    for payload in events.draft_notifies {
        fan_out_team_draft_notify(state, payload).await;
    }
    for payload in events.shuffle_suggestion_notifies {
        fan_out_team_member_shuffle_suggestion(state, payload).await;
    }
}

async fn fan_out_team_member_shuffle_suggestion(
    state: &mut HostState,
    payload: TeamMemberShuffleSuggestionNotifyPayload,
) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_shuffle_suggestion_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_notify(state: &mut HostState, payload: TeamNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_member_notify(state: &mut HostState, payload: TeamMemberNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_member_binding_notify(
    state: &mut HostState,
    payload: TeamMemberBindingNotifyPayload,
) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_binding_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_draft_notify(state: &mut HostState, payload: TeamDraftNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_draft_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn agent_team_validation_refs(
    state: &HostState,
    operation: &'static str,
) -> AppResult<AgentTeamValidationRefs> {
    let custom_agent_ids = state
        .custom_agent_store
        .lock()
        .await
        .list()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .into_iter()
        .map(|custom_agent| custom_agent.id)
        .collect::<HashSet<_>>();
    let project_ids = state
        .project_store
        .lock()
        .await
        .list()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .into_iter()
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    let enabled_backend_kinds = state
        .settings_store
        .lock()
        .await
        .get()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .enabled_backends
        .into_iter()
        .collect::<HashSet<_>>();
    let (role_preset_ids, personality_preset_ids) = team_preset_validation_refs();
    Ok(AgentTeamValidationRefs {
        custom_agent_ids,
        project_ids,
        enabled_backend_kinds,
        role_preset_ids,
        personality_preset_ids,
        legacy_backend_kind: None,
        purged_gemini_session_ids: HashSet::new(),
    })
}

fn project_stream_path(project_id: &ProjectId) -> StreamPath {
    StreamPath(format!("/project/{}", project_id.0))
}

async fn ensure_project_actor(
    state: &mut HostState,
    project_id: ProjectId,
) -> Result<ProjectStreamHandle, String> {
    if let Some(subscription) = state.project_streams.get(&project_id)
        && !subscription.task.is_finished()
    {
        return Ok(subscription.handle.clone());
    }

    if let Some(subscription) = state.project_streams.remove(&project_id) {
        subscription.task.abort();
    }

    let subscription = spawn_project_subscription(
        Arc::clone(&state.project_store),
        project_id.clone(),
        state.review_registry.clone(),
    )
    .await?;
    let handle = subscription.handle.clone();
    state.project_streams.insert(project_id, subscription);
    Ok(handle)
}

async fn subscribe_host_to_project(
    state: &mut HostState,
    host_path: &StreamPath,
    project_id: ProjectId,
) -> Result<(), String> {
    let Some(subscriber) = state.host_streams.get(host_path) else {
        return Ok(());
    };
    let project_output_stream = subscriber
        .stream
        .with_path(project_stream_path(&project_id));
    let summaries = state.review_registry.summaries(project_id.clone()).await?;
    let handle = ensure_project_actor(state, project_id).await?;
    handle
        .add_subscriber(host_path.clone(), project_output_stream, summaries)
        .await
}

async fn emit_review_list_changed_for_project(
    state: &mut HostState,
    project_id: ProjectId,
) -> Result<(), String> {
    let summaries = state.review_registry.summaries(project_id.clone()).await?;
    let handle = ensure_project_actor(state, project_id).await?;
    handle
        .emit_project_event(protocol::ProjectEventPayload::ReviewListChanged { reviews: summaries })
        .await
}

async fn fan_out_host_settings(state: &mut HostState, settings: protocol::HostSettings) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_host_settings_for_subscriber(&settings, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_session_schemas(state: &mut HostState) {
    let enabled_backends = state
        .settings_store
        .lock()
        .await
        .get()
        .unwrap_or_else(|err| panic!("failed to load host settings for session schemas: {err}"))
        .enabled_backends;
    let schemas = session_schemas_for_enabled_backends(state, &enabled_backends);
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_session_schemas_for_subscriber(&schemas, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_backend_setup(state: &mut HostState, payload: BackendSetupPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_backend_setup_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn emit_team_notify_for_subscriber(
    payload: &TeamNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamNotify payload for host stream fanout");
    subscriber.stream.send_value(FrameKind::TeamNotify, payload)
}

async fn emit_team_member_notify_for_subscriber(
    payload: &TeamMemberNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamMemberNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::TeamMemberNotify, payload)
}

async fn emit_team_member_binding_notify_for_subscriber(
    payload: &TeamMemberBindingNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamMemberBindingNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::TeamMemberBindingNotify, payload)
}

async fn emit_team_draft_notify_for_subscriber(
    payload: &TeamDraftNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamDraftNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::TeamDraftNotify, payload)
}

async fn emit_team_member_shuffle_suggestion_for_subscriber(
    payload: &TeamMemberShuffleSuggestionNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload).expect(
        "failed to serialize TeamMemberShuffleSuggestionNotify payload for host stream fanout",
    );
    subscriber
        .stream
        .send_value(FrameKind::TeamMemberShuffleSuggestionNotify, payload)
}

async fn emit_project_notify_for_subscriber(
    payload: &ProjectNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize ProjectNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::ProjectNotify, payload)
}

async fn emit_custom_agent_notify_for_subscriber(
    payload: &CustomAgentNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize CustomAgentNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::CustomAgentNotify, payload)
}

async fn emit_steering_notify_for_subscriber(
    payload: &SteeringNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize SteeringNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::SteeringNotify, payload)
}

async fn emit_skill_notify_for_subscriber(
    payload: &SkillNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize SkillNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::SkillNotify, payload)
}

async fn emit_mcp_server_notify_for_subscriber(
    payload: &McpServerNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize McpServerNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::McpServerNotify, payload)
}

async fn emit_host_settings_for_subscriber(
    settings: &protocol::HostSettings,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(HostSettingsPayload {
        settings: settings.clone(),
    })
    .expect("failed to serialize HostSettings payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::HostSettings, payload)
}

async fn emit_backend_setup_for_subscriber(
    payload: &BackendSetupPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize BackendSetup payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::BackendSetup, payload)
}

async fn emit_agent_closed_for_stream(
    payload: &AgentClosedPayload,
    stream: &Stream,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize AgentClosed payload for host stream fanout");
    stream.send_value(FrameKind::AgentClosed, payload)
}

async fn emit_session_schemas_for_subscriber(
    schemas: &[SessionSchemaEntry],
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    if subscriber.last_session_schemas.as_deref() == Some(schemas) {
        return Ok(());
    }
    let payload = serde_json::to_value(SessionSchemasPayload {
        schemas: schemas.to_vec(),
    })
    .expect("failed to serialize SessionSchemas payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::SessionSchemas, payload)?;
    subscriber.last_session_schemas = Some(schemas.to_vec());
    Ok(())
}

fn session_schema_for_backend(
    state: &HostState,
    backend_kind: protocol::BackendKind,
) -> Option<SessionSettingsSchema> {
    match backend_kind {
        protocol::BackendKind::Kiro => match &state.kiro_session_schema {
            KiroSessionSchemaState::Ready(schema) => Some(schema.clone()),
            KiroSessionSchemaState::Pending | KiroSessionSchemaState::Unavailable(_) => None,
        },
        _ => Some(session_settings_schema_for_backend(backend_kind)),
    }
}

fn session_schema_entry_for_backend(
    state: &HostState,
    backend_kind: protocol::BackendKind,
) -> SessionSchemaEntry {
    match backend_kind {
        protocol::BackendKind::Kiro => match &state.kiro_session_schema {
            KiroSessionSchemaState::Ready(schema) => SessionSchemaEntry::Ready {
                schema: schema.clone(),
            },
            KiroSessionSchemaState::Pending => SessionSchemaEntry::Pending { backend_kind },
            KiroSessionSchemaState::Unavailable(message) => SessionSchemaEntry::Unavailable {
                backend_kind,
                message: message.clone(),
            },
        },
        _ => SessionSchemaEntry::Ready {
            schema: session_settings_schema_for_backend(backend_kind),
        },
    }
}

fn session_schemas_for_enabled_backends(
    state: &HostState,
    enabled_backends: &[protocol::BackendKind],
) -> Vec<SessionSchemaEntry> {
    enabled_backends
        .iter()
        .copied()
        .map(|backend_kind| session_schema_entry_for_backend(state, backend_kind))
        .collect()
}

fn kiro_probe_workspace_root() -> Result<String, String> {
    Ok(crate::paths::home_dir()?.to_string_lossy().into_owned())
}

fn new_instance_stream(agent_id: &AgentId) -> StreamPath {
    let instance_id = Uuid::new_v4();
    StreamPath(format!("/agent/{}/{}", agent_id, instance_id))
}

fn default_compaction_summary_prompt() -> String {
    "Summarize the durable context a future replacement Tyde agent should remember. \
Focus on user preferences, project facts, decisions, constraints, open threads, and \
useful debugging or implementation learnings. Omit transient chatter and output only \
the summary."
        .to_owned()
}

fn build_compaction_replacement_prompt(summary: &str) -> String {
    format!(
        "You are replacing a previous persistent Tyde agent after session compaction. \
Use this memory summary as your durable context, but do not claim you can access the \
old session transcript unless the user provides it.\n\nMemory summary:\n{summary}\n\n\
Acknowledge briefly and continue from this memory."
    )
}

fn compaction_summary_preview(summary: &str) -> String {
    summary
        .chars()
        .take(COMPACTION_SUMMARY_PREVIEW_CHARS)
        .collect()
}

fn send_agent_compact_notify(stream: &Stream, payload: AgentCompactNotifyPayload) {
    let value = serde_json::to_value(payload)
        .expect("failed to serialize AgentCompactNotify payload for agent stream");
    let _ = stream.send_value(FrameKind::AgentCompactNotify, value);
}

fn drain_final_agent_compact_notify(
    old_agent_id: &AgentId,
    rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
) -> AgentCompactNotifyPayload {
    let mut final_notify = None;
    while let Ok(envelope) = rx.try_recv() {
        if envelope.kind != FrameKind::AgentCompactNotify {
            continue;
        }
        match envelope.parse_payload::<AgentCompactNotifyPayload>() {
            Ok(payload)
                if matches!(
                    payload.status,
                    AgentCompactStatus::Completed | AgentCompactStatus::Failed
                ) =>
            {
                final_notify = Some(payload);
            }
            Ok(_) => {}
            Err(error) => {
                return AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: old_agent_id.clone(),
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!("failed to parse compaction result: {error}")),
                };
            }
        }
    }

    final_notify.unwrap_or(AgentCompactNotifyPayload {
        status: AgentCompactStatus::Failed,
        old_agent_id: old_agent_id.clone(),
        old_session_id: None,
        new_agent_id: None,
        new_session_id: None,
        summary_preview: None,
        message: Some("agent compaction finished without a terminal notify".to_owned()),
    })
}

fn send_team_compact_notify(stream: &Stream, payload: TeamCompactNotifyPayload) {
    let value = serde_json::to_value(payload)
        .expect("failed to serialize TeamCompactNotify payload for host stream");
    let _ = stream.send_value(FrameKind::TeamCompactNotify, value);
}

impl HostHandle {
    async fn refresh_after_project_mutation(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        _path: Option<ProjectPath>,
    ) -> AppResult<()> {
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                &project_output_stream,
                project_id,
                "project_mutation_refresh",
            )
            .await?;
        handle
            .refresh()
            .await
            .map_err(|error| project_command_error("project_mutation_refresh", error))?;
        Ok(())
    }

    async fn terminal_handle(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) -> AppResult<TerminalHandle> {
        let state = self.state.lock().await;
        state
            .terminal_streams
            .get(&(connection_host_stream.clone(), terminal_id.clone()))
            .cloned()
            .ok_or_else(|| {
                AppError::not_found(
                    "terminal_lookup",
                    format!(
                        "terminal {} is not owned by host stream {}",
                        terminal_id, connection_host_stream
                    ),
                )
            })
    }
}

async fn resolve_terminal_launch(
    project_store: &Arc<Mutex<ProjectStore>>,
    payload: TerminalCreatePayload,
) -> AppResult<TerminalLaunchInfo> {
    const OPERATION: &str = "terminal_create";
    match payload.target {
        TerminalLaunchTarget::HostDefault => {
            let cwd = std::env::current_dir()
                .context("failed to resolve host default cwd")
                .map_err(|error| AppError::internal(OPERATION, error))?
                .display()
                .to_string();
            Ok(TerminalLaunchInfo {
                project_id: None,
                root: None,
                cwd,
                cols: payload.cols,
                rows: payload.rows,
            })
        }
        TerminalLaunchTarget::Project {
            project_id,
            root,
            relative_cwd,
        } => {
            let project = load_project(project_store, &project_id, OPERATION).await?;
            let roots = project.root_paths().into_iter().collect::<HashSet<_>>();
            if !roots.contains(&root) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "cannot create terminal in root {} that is not part of project {}",
                        root, project_id
                    ),
                ));
            }

            let cwd = resolve_project_terminal_cwd(&root, relative_cwd.as_deref())
                .map_err(|error| AppError::invalid(OPERATION, error))?;
            Ok(TerminalLaunchInfo {
                project_id: Some(project_id),
                root: Some(root),
                cwd,
                cols: payload.cols,
                rows: payload.rows,
            })
        }
        TerminalLaunchTarget::Path { cwd } => {
            let trimmed = cwd.trim();
            if trimmed.is_empty() {
                return Err(AppError::invalid(
                    OPERATION,
                    "terminal path cwd must not be empty",
                ));
            }
            if !Path::new(trimmed).is_absolute() {
                return Err(AppError::invalid(
                    OPERATION,
                    format!("terminal path cwd must be absolute: {}", trimmed),
                ));
            }
            Ok(TerminalLaunchInfo {
                project_id: None,
                root: None,
                cwd: trimmed.to_owned(),
                cols: payload.cols,
                rows: payload.rows,
            })
        }
    }
}

fn resolve_project_terminal_cwd(
    root: &ProjectRootPath,
    relative_cwd: Option<&str>,
) -> Result<String, String> {
    let Some(relative_cwd) = relative_cwd else {
        return Ok(root.0.clone());
    };
    validate_terminal_relative_path(relative_cwd)?;
    Ok(Path::new(&root.0)
        .join(relative_cwd)
        .to_string_lossy()
        .to_string())
}

fn validate_terminal_relative_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("terminal relative_cwd must not be empty".to_owned());
    }

    let relative = Path::new(path);
    if !relative.is_relative() {
        return Err(format!("terminal relative_cwd must be relative: {}", path));
    }

    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "terminal relative_cwd must not contain '..': {}",
                    path
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("terminal relative_cwd must be relative: {}", path));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::{MOCK_DIE_AFTER_BUSY_SENTINEL, MOCK_SLOW_TURN_SENTINEL};
    use crate::review::ReviewHandle;
    use crate::store::agent_teams::AgentTeamsStoreFile;
    use protocol::{
        AgentErrorPayload, BackendKind, CustomAgentId, DiffContextMode, HostSettingValue,
        ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload, ProtocolValidator, Review,
        ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewStatus, TeamMemberCreateSpec,
        ToolPolicy,
    };

    #[test]
    fn startup_mcp_servers_attach_config_only_to_help_agent() {
        let settings = protocol::HostSettings {
            enabled_backends: vec![BackendKind::Claude],
            default_backend: Some(BackendKind::Claude),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: false,
            complexity_tiers_enabled: false,
            backend_tier_configs: HashMap::new(),
        };
        let debug_mcp = DebugMcpHandle { url: String::new() };
        let agent_control = AgentControlMcpHandle { url: String::new() };
        let config_mcp = ConfigMcpHandle {
            url: "http://127.0.0.1:9/mcp".to_owned(),
        };

        let help_id = CustomAgentId(crate::store::custom_agents::HELP_CUSTOM_AGENT_ID.to_owned());
        let servers = startup_mcp_servers_for_settings(
            &settings,
            &[],
            &debug_mcp,
            &agent_control,
            &config_mcp,
            Some(&help_id),
        );
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["tyde-config"],
            "help agent must get the config tools"
        );

        let other_id = CustomAgentId("ca-user".to_owned());
        for custom_agent_id in [None, Some(&other_id)] {
            let servers = startup_mcp_servers_for_settings(
                &settings,
                &[],
                &debug_mcp,
                &agent_control,
                &config_mcp,
                custom_agent_id,
            );
            assert!(
                servers.iter().all(|s| s.name != "tyde-config"),
                "non-help spawns must not get config tools: {custom_agent_id:?}"
            );
        }
    }

    #[test]
    fn antigravity_session_summary_resumability_requires_native_db() {
        let missing_id = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_owned());
        let existing_id = SessionId("66666666-6666-4666-8666-666666666666".to_owned());
        let compacted_id = SessionId("77777777-7777-4777-8777-777777777777".to_owned());
        let backend_native_id = SessionId("88888888-8888-4888-8888-888888888888".to_owned());
        let replacement_id = SessionId("99999999-9999-4999-8999-999999999999".to_owned());
        let parent_id = SessionId("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned());
        let synthetic_id = SessionId("antigravity-legacy".to_owned());
        let claude_id = SessionId("claude-session".to_owned());
        let mut compacted_summary =
            test_session_summary(BackendKind::Antigravity, compacted_id.clone(), false);
        compacted_summary.compacted_to_session_id = Some(replacement_id);
        let mut backend_native_summary =
            test_session_summary(BackendKind::Antigravity, backend_native_id.clone(), false);
        backend_native_summary.parent_id = Some(parent_id);
        let mut summaries = vec![
            test_session_summary(BackendKind::Antigravity, missing_id.clone(), true),
            test_session_summary(BackendKind::Antigravity, existing_id.clone(), false),
            compacted_summary,
            backend_native_summary,
            test_session_summary(BackendKind::Antigravity, synthetic_id, true),
            test_session_summary(BackendKind::Claude, claude_id.clone(), true),
        ];

        normalize_antigravity_session_resumability_with(&mut summaries, |session_id| {
            session_id == &existing_id
                || session_id == &compacted_id
                || session_id == &backend_native_id
        });

        assert!(!summaries[0].resumable);
        assert!(summaries[1].resumable);
        assert!(!summaries[2].resumable);
        assert!(!summaries[3].resumable);
        assert!(!summaries[4].resumable);
        assert!(summaries[5].resumable);
    }

    fn test_session_summary(
        backend_kind: BackendKind,
        id: SessionId,
        resumable: bool,
    ) -> SessionSummary {
        SessionSummary {
            id,
            backend_kind,
            workspace_roots: Vec::new(),
            project_id: None,
            alias: None,
            user_alias: None,
            parent_id: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 0,
            token_count: None,
            resumable,
            compacted_from_session_id: None,
            compacted_to_session_id: None,
            compacted_at_ms: None,
            compaction_summary_preview: None,
        }
    }

    #[test]
    fn spawn_host_with_mock_backend_does_not_require_existing_tokio_runtime() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");

        let result = spawn_host_with_mock_backend(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
        );
        if let Err(err) = result {
            panic!("host spawn should succeed without an existing Tokio runtime: {err}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_path_percent_encodes_branch_bytes() {
        let parent = ProjectRootPath("/Users/mike/Tyde2".to_owned());

        let simple = compute_worktree_path(&parent, &GitBranchName("feature-login".to_owned()))
            .expect("compute simple worktree path");
        assert_eq!(simple.0, "/Users/mike/Tyde2--feature-login");

        let slash = compute_worktree_path(&parent, &GitBranchName("feature/login".to_owned()))
            .expect("compute slash worktree path");
        assert_eq!(slash.0, "/Users/mike/Tyde2--feature%2Flogin");

        let unicode = compute_worktree_path(&parent, &GitBranchName("café%".to_owned()))
            .expect("compute unicode worktree path");
        assert_eq!(unicode.0, "/Users/mike/Tyde2--caf%C3%A9%25");
    }

    #[tokio::test]
    async fn workbench_remove_reports_internal_when_parent_record_is_missing() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_mock_backend(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
        )
        .expect("spawn host");

        let workbench = Project {
            id: ProjectId("workbench-test".to_owned()),
            name: "feature".to_owned(),
            sort_order: 0,
            source: ProjectSource::GitWorkbench {
                parent_project_id: ProjectId("missing-parent".to_owned()),
                branch: GitBranchName("feature".to_owned()),
                roots: vec![WorkbenchRoot {
                    parent_root: ProjectRootPath("/tmp/parent".to_owned()),
                    worktree_root: ProjectRootPath("/tmp/parent--feature".to_owned()),
                }],
            },
        };

        let error = host
            .validate_workbench_remove_blockers(&workbench)
            .await
            .expect_err("missing parent should block removal");
        assert_eq!(error.code(), protocol::CommandErrorCode::Internal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct TeamFixture {
        _dir: tempfile::TempDir,
        host: HostHandle,
        team: protocol::Team,
        manager: TeamMember,
        report: TeamMember,
        custom_agent_id: CustomAgentId,
        project_id: ProjectId,
        project_root: String,
        agent_team_store_path: PathBuf,
    }

    struct TeamRaceFixture {
        temp_dir: tempfile::TempDir,
        host: HostHandle,
        team: protocol::Team,
        custom_agent_id: CustomAgentId,
        project_id: ProjectId,
        agent_team_store_path: PathBuf,
    }

    struct CompactFixture {
        _dir: tempfile::TempDir,
        host: HostHandle,
    }

    async fn team_fixture() -> TeamFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_root = dir.path().join("project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let session_path = dir.path().join("sessions.json");
        let project_path = dir.path().join("projects.json");
        let settings_path = dir.path().join("settings.json");
        let agent_team_store_path = dir.path().join("agent_teams.json");
        let host = spawn_host_with_mock_backend(session_path, project_path, settings_path)
            .expect("spawn mock host");

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");

        let custom_agent_id = CustomAgentId(format!("custom-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: custom_agent_id.clone(),
                name: "Team Custom Agent".to_owned(),
                description: "Handles team work".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert custom agent");

        host.create_project(ProjectCreatePayload {
            name: "Team Project".to_owned(),
            roots: vec![ProjectRootPath(project_root.to_string_lossy().to_string())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Team Project")
                .expect("created project")
                .id
        };

        host.create_team(TeamCreatePayload {
            name: "Product Team".to_owned(),
            manager: TeamMemberCreateSpec {
                name: "Manager".to_owned(),
                description: "Coordinates reports".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id.clone()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![project_id.clone()],
            },
        })
        .await
        .expect("create team");
        let (team, manager) = {
            let snapshot = team_snapshot(&host).await;
            let team = snapshot
                .teams
                .into_iter()
                .find(|team| team.name == "Product Team")
                .expect("created team");
            let manager = snapshot
                .members
                .into_iter()
                .find(|member| member.id == team.manager_member_id)
                .expect("created manager");
            (team, manager)
        };

        host.create_team_member(TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: TeamMemberCreateSpec {
                name: "Report".to_owned(),
                description: "Implements delegated work".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id.clone()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![project_id.clone()],
            },
            session_id: None,
        })
        .await
        .expect("create report");
        let report = team_snapshot(&host)
            .await
            .members
            .into_iter()
            .find(|member| member.team_id == team.id && member.role == TeamMemberRole::Report)
            .expect("created report");

        TeamFixture {
            _dir: dir,
            host,
            team,
            manager,
            report,
            custom_agent_id,
            project_id,
            project_root: project_root.to_string_lossy().to_string(),
            agent_team_store_path,
        }
    }

    async fn team_race_fixture() -> TeamRaceFixture {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let project_root = temp_dir.path().join("race-project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let project_root = project_root.to_string_lossy().to_string();
        let manager_project_root = temp_dir.path().join("race-manager-project-root");
        std::fs::create_dir_all(&manager_project_root).expect("create manager project root");
        let manager_project_root = manager_project_root.to_string_lossy().to_string();
        let session_path = temp_dir.path().join("sessions.json");
        let project_path = temp_dir.path().join("projects.json");
        let settings_path = temp_dir.path().join("settings.json");
        let agent_team_store_path = temp_dir.path().join("agent_teams.json");
        let host = spawn_host_with_mock_backend(session_path, project_path, settings_path)
            .expect("spawn mock host");

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");

        let manager_custom_agent_id = CustomAgentId(format!("manager-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: manager_custom_agent_id.clone(),
                name: "Race Manager Custom Agent".to_owned(),
                description: "Owns the team manager".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert manager custom agent");

        let custom_agent_id = CustomAgentId(format!("race-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: custom_agent_id.clone(),
                name: "Race Custom Agent".to_owned(),
                description: "The custom agent raced with member creation".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert raced custom agent");

        host.create_project(ProjectCreatePayload {
            name: "Race Manager Project".to_owned(),
            roots: vec![ProjectRootPath(manager_project_root)],
        })
        .await
        .expect("create manager project");
        let manager_project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Race Manager Project")
                .expect("created manager project")
                .id
        };

        host.create_project(ProjectCreatePayload {
            name: "Race Project".to_owned(),
            roots: vec![ProjectRootPath(project_root.clone())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Race Project")
                .expect("created project")
                .id
        };

        host.create_team(TeamCreatePayload {
            name: "Race Team".to_owned(),
            manager: TeamMemberCreateSpec {
                name: "Race Manager".to_owned(),
                description: "Coordinates the race test".to_owned(),
                profile: None,
                custom_agent_id: Some(manager_custom_agent_id),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![manager_project_id],
            },
        })
        .await
        .expect("create team");
        let team = team_snapshot(&host)
            .await
            .teams
            .into_iter()
            .find(|team| team.name == "Race Team")
            .expect("created team");

        TeamRaceFixture {
            temp_dir,
            host,
            team,
            custom_agent_id,
            project_id,
            agent_team_store_path,
        }
    }

    async fn team_snapshot(host: &HostHandle) -> crate::team_registry::TeamRegistrySnapshot {
        let registry = { host.state.lock().await.team_registry.clone() };
        registry.snapshot().await.expect("team snapshot")
    }

    async fn bind_team_member(host: &HostHandle, member: &TeamMember) -> AgentId {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        let session_id = SessionId(format!("session-{}", Uuid::new_v4()));
        let (registry, refs) = {
            let state = host.state.lock().await;
            (
                state.team_registry.clone(),
                agent_team_validation_refs(&state, "test_bind_team_member")
                    .await
                    .expect("team refs"),
            )
        };
        let events = registry
            .bind_member_agent(member.id.clone(), agent_id.clone(), Some(session_id), refs)
            .await
            .expect("bind member");
        host.fan_out_team_registry_events(events).await;
        agent_id
    }

    fn member_from_snapshot(
        host_snapshot: crate::team_registry::TeamRegistrySnapshot,
        id: &TeamMemberId,
    ) -> TeamMember {
        host_snapshot
            .members
            .into_iter()
            .find(|member| member.id == *id)
            .expect("member in snapshot")
    }

    fn persisted_team_store(path: &Path) -> AgentTeamsStoreFile {
        let json = std::fs::read_to_string(path).expect("read agent teams store");
        serde_json::from_str(&json).expect("parse agent teams store")
    }

    fn assert_no_team_member_references_project(
        store: &AgentTeamsStoreFile,
        project_id: &ProjectId,
    ) {
        for member in store.members.values() {
            assert!(
                !member.project_ids.contains(project_id),
                "member {} still references deleted project {}",
                member.id,
                project_id
            );
        }
    }

    async fn assert_agent_team_store_loads_with_current_refs(host: &HostHandle, path: &Path) {
        let refs = {
            let state = host.state.lock().await;
            agent_team_validation_refs(&state, "test_validate_agent_team_store")
                .await
                .expect("team refs")
        };
        AgentTeamsStore::load(path.to_path_buf(), &refs).expect("agent teams store validates");
    }

    fn team_member_create_payload(
        fixture: &TeamRaceFixture,
        custom_agent_id: CustomAgentId,
        project_ids: Vec<ProjectId>,
    ) -> TeamMemberCreatePayload {
        TeamMemberCreatePayload {
            team_id: fixture.team.id.clone(),
            member: TeamMemberCreateSpec {
                name: "Race Report".to_owned(),
                description: "Created while a referenced record is deleting".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids,
            },
            session_id: None,
        }
    }

    fn team_mutation_race_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn wait_for_team_member_unbound(host: &HostHandle, member_id: &TeamMemberId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = team_snapshot(host).await;
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            if binding.current_agent_id.is_none() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for team member {member_id} to unbind"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_team_member_binding_idle(host: &HostHandle, member_id: &TeamMemberId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = team_snapshot(host).await;
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            if binding.current_agent_id.is_some() && binding.status == AgentControlStatus::Idle {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for team member {member_id} binding to become idle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn compact_fixture() -> CompactFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("spawn mock host");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");
        CompactFixture { _dir: dir, host }
    }

    async fn spawn_idle_user_agent(host: &HostHandle, prompt: &str) -> (AgentId, SessionId) {
        let agent_id = host
            .spawn_agent(SpawnAgentPayload {
                name: Some("Compact Me".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: Vec::new(),
                    prompt: prompt.to_owned(),
                    images: None,
                    backend_kind: BackendKind::Claude,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await;
        let session_id = host
            .wait_for_agent_session_id_result(&agent_id)
            .await
            .expect("agent session id");
        wait_for_agent_idle(host, &agent_id).await;
        (agent_id, session_id)
    }

    async fn wait_for_agent_idle(host: &HostHandle, agent_id: &AgentId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = host.agent_status_snapshot(agent_id).await
                && status.started
                && !status.terminated
                && !status.is_active()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for agent {agent_id} to become idle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_agent_active(host: &HostHandle, agent_id: &AgentId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = host.agent_status_snapshot(agent_id).await
                && status.started
                && !status.terminated
                && status.is_active()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for agent {agent_id} to become active"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn compact_stream(agent_id: &AgentId) -> (Stream, mpsc::UnboundedReceiver<protocol::Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Stream::new(StreamPath(format!("/agent/{}", agent_id)), tx),
            rx,
        )
    }

    fn drain_validated_events(
        validator: &mut ProtocolValidator,
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
        context: &str,
    ) -> Vec<protocol::Envelope> {
        let mut envelopes = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            validator
                .validate_envelope(&envelope)
                .unwrap_or_else(|err| {
                    panic!(
                        "{context}: protocol violation after {} on {}: {err}",
                        envelope.kind, envelope.stream
                    )
                });
            envelopes.push(envelope);
        }
        envelopes
    }

    fn drain_compact_notifies(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> Vec<AgentCompactNotifyPayload> {
        let mut notifies = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            if envelope.kind == FrameKind::AgentCompactNotify {
                notifies.push(
                    envelope
                        .parse_payload::<AgentCompactNotifyPayload>()
                        .expect("compact notify payload"),
                );
            }
        }
        notifies
    }

    #[tokio::test]
    async fn agent_compaction_route_does_not_block_host_commands() {
        let fixture = compact_fixture().await;
        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember routing responsiveness").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/compact-route-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path.clone(), tx);
        let agent_path = StreamPath(format!("/agent/{}/{}", old_agent_id, Uuid::new_v4()));
        let compact = protocol::Envelope::from_payload(
            agent_path,
            FrameKind::AgentCompact,
            0,
            &AgentCompactPayload {
                summary_prompt: Some(format!("{MOCK_SLOW_TURN_SENTINEL} summarize")),
                max_summary_bytes: None,
            },
        )
        .expect("compact envelope");

        tokio::time::timeout(
            Duration::from_millis(200),
            crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, compact),
        )
        .await
        .expect("agent_compact route should return without waiting for compaction")
        .expect("route compact");

        let list_sessions = protocol::Envelope::from_payload(
            host_path.clone(),
            FrameKind::ListSessions,
            0,
            &protocol::ListSessionsPayload::default(),
        )
        .expect("list sessions envelope");
        tokio::time::timeout(
            Duration::from_millis(200),
            crate::router::route_client_envelope(
                &fixture.host,
                &host_path,
                &host_stream,
                list_sessions,
            ),
        )
        .await
        .expect("list_sessions route should not wait behind compaction")
        .expect("route list sessions");

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("session list should be emitted while compaction is still running")
                .expect("output envelope");
            if envelope.kind == FrameKind::SessionList {
                break;
            }
            if envelope.kind == FrameKind::AgentCompactNotify {
                let payload: AgentCompactNotifyPayload =
                    envelope.parse_payload().expect("compact notify");
                assert_ne!(
                    payload.status,
                    AgentCompactStatus::Completed,
                    "slow compaction completed before ListSessions was processed"
                );
            }
        }

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("background compaction should finish")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentCompactNotify {
                continue;
            }
            let payload: AgentCompactNotifyPayload =
                envelope.parse_payload().expect("compact notify");
            if matches!(
                payload.status,
                AgentCompactStatus::Completed | AgentCompactStatus::Failed
            ) {
                assert_eq!(payload.status, AgentCompactStatus::Completed);
                break;
            }
        }
    }

    #[tokio::test]
    async fn agent_compaction_route_orders_later_agent_input_after_compact() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/compact-ordering-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path.clone(), tx);
        assert!(
            fixture
                .host
                .register_host_stream(host_stream.clone())
                .await
                .is_empty()
        );

        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember input ordering").await;
        let old_instance_stream = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("new agent should be emitted")
                .expect("output envelope");
            if envelope.kind != FrameKind::NewAgent {
                continue;
            }
            let payload: NewAgentPayload = envelope.parse_payload().expect("new agent payload");
            if payload.agent_id == old_agent_id {
                break payload.instance_stream;
            }
        };

        let compact = protocol::Envelope::from_payload(
            old_instance_stream.clone(),
            FrameKind::AgentCompact,
            0,
            &AgentCompactPayload {
                summary_prompt: Some(format!("{MOCK_SLOW_TURN_SENTINEL} summarize")),
                max_summary_bytes: None,
            },
        )
        .expect("compact envelope");
        crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, compact)
            .await
            .expect("route compact");

        let send = protocol::Envelope::from_payload(
            old_instance_stream,
            FrameKind::SendMessage,
            1,
            &SendMessagePayload {
                message: "This input must wait behind compaction".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .expect("send envelope");
        crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, send)
            .await
            .expect("route send");

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("compacting agent should reject later input")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentError {
                continue;
            }
            let payload: AgentErrorPayload = envelope.parse_payload().expect("agent error payload");
            if payload.agent_id == old_agent_id
                && payload.message.contains("compaction is in progress")
            {
                break;
            }
        }

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("background compaction should finish")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentCompactNotify {
                continue;
            }
            let payload: AgentCompactNotifyPayload =
                envelope.parse_payload().expect("compact notify");
            if matches!(
                payload.status,
                AgentCompactStatus::Completed | AgentCompactStatus::Failed
            ) {
                assert_eq!(payload.status, AgentCompactStatus::Completed);
                break;
            }
        }
    }

    #[tokio::test]
    async fn agent_compaction_rotates_user_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "remember this user preference").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(old_agent_id.clone(), AgentCompactPayload::default(), stream)
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        assert_eq!(
            notifies.first().map(|notify| notify.status),
            Some(AgentCompactStatus::Started)
        );
        let completed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Completed)
            .expect("completed notify");
        let new_agent_id = completed
            .new_agent_id
            .clone()
            .expect("new agent id in completed notify");
        let new_session_id = completed
            .new_session_id
            .clone()
            .expect("new session id in completed notify");
        assert_ne!(new_agent_id, old_agent_id);
        assert_ne!(new_session_id, old_session_id);
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_none());
        assert!(fixture.host.agent_handle(&new_agent_id).await.is_some());

        let (old_record, new_record) = {
            let state = fixture.host.state.lock().await;
            let store = state.session_store.lock().await;
            (
                store.get(&old_session_id).expect("old record"),
                store.get(&new_session_id).expect("new record"),
            )
        };
        assert!(!old_record.resumable);
        assert_eq!(
            old_record.compacted_to_session_id.as_ref(),
            Some(&new_session_id)
        );
        assert_eq!(
            new_record.compacted_from_session_id.as_ref(),
            Some(&old_session_id)
        );
        assert!(
            old_record
                .compaction_summary_preview
                .as_deref()
                .is_some_and(|preview| preview.contains("mock backend response"))
        );
    }

    #[tokio::test]
    async fn agent_compaction_completed_validates_before_old_agent_closed_on_instance_stream() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_stream = Stream::new(
            StreamPath(format!("/host/compact-order-{}", Uuid::new_v4())),
            tx.clone(),
        );
        assert!(
            fixture
                .host
                .register_host_stream(host_stream)
                .await
                .is_empty()
        );
        let mut validator = ProtocolValidator::new();
        drain_validated_events(&mut validator, &mut rx, "host registration");

        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember protocol ordering").await;
        let spawn_events = drain_validated_events(&mut validator, &mut rx, "old agent spawn");
        let old_instance_stream = spawn_events
            .iter()
            .find_map(|envelope| {
                if envelope.kind != FrameKind::NewAgent {
                    return None;
                }
                let payload: NewAgentPayload =
                    envelope.parse_payload().expect("parse old NewAgent");
                (payload.agent_id == old_agent_id).then_some(payload.instance_stream)
            })
            .expect("old agent instance stream");
        assert!(
            old_instance_stream
                .0
                .starts_with(&format!("/agent/{}/", old_agent_id))
        );

        let compact_stream = Stream::new(old_instance_stream.clone(), tx);
        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload::default(),
                compact_stream,
            )
            .await
            .expect("compact agent");

        let compact_events = drain_validated_events(&mut validator, &mut rx, "agent compaction");
        let mut completed_index = None;
        let mut closed_index = None;
        for (index, envelope) in compact_events.iter().enumerate() {
            match envelope.kind {
                FrameKind::AgentCompactNotify if envelope.stream == old_instance_stream => {
                    let payload: AgentCompactNotifyPayload =
                        envelope.parse_payload().expect("compact notify");
                    if payload.status == AgentCompactStatus::Completed {
                        completed_index = Some(index);
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("agent closed");
                    if payload.agent_id == old_agent_id {
                        closed_index = Some(index);
                    }
                }
                _ => {}
            }
        }
        let completed_index = completed_index.expect("completed compact notify");
        let closed_index = closed_index.expect("old AgentClosed");
        assert!(
            completed_index < closed_index,
            "Completed notify must arrive before AgentClosed invalidates the old stream"
        );
    }

    #[tokio::test]
    async fn agent_compaction_rejects_busy_agent() {
        let fixture = compact_fixture().await;
        let resolved = ResolvedSpawnConfig {
            instructions: Some(MOCK_SLOW_TURN_SENTINEL.to_owned()),
            ..Default::default()
        };
        let agent_id = fixture
            .host
            .spawn_agent_with_origin_config_and_team(
                SpawnAgentPayload {
                    name: Some("Busy Agent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "slow start".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                },
                AgentOrigin::User,
                Some(resolved),
                None,
            )
            .await;
        let old_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&agent_id)
            .await
            .expect("agent session id");
        wait_for_agent_active(&fixture.host, &agent_id).await;
        let (stream, mut rx) = compact_stream(&agent_id);

        fixture
            .host
            .compact_agent(agent_id.clone(), AgentCompactPayload::default(), stream)
            .await
            .expect("compact busy agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("busy"))
        );
        assert!(fixture.host.agent_handle(&agent_id).await.is_some());
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_summary_failure_leaves_old_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "initial prompt").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload {
                    summary_prompt: Some("/compact".to_owned()),
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("summary was empty"))
        );
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_some());
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_replacement_failure_leaves_old_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "initial prompt").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload {
                    summary_prompt: Some("__mock_fail_spawn__".to_owned()),
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("replacement agent failed"))
        );
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_some());
        if let Some(new_agent_id) = failed.new_agent_id.as_ref() {
            assert!(fixture.host.agent_handle(new_agent_id).await.is_none());
        }
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_rotates_team_member_session() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");
        let old_report_agent_id = outcome.agent_id;
        let old_report_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&old_report_agent_id)
            .await
            .expect("report session");
        wait_for_agent_idle(&fixture.host, &old_report_agent_id).await;
        let (stream, mut rx) = compact_stream(&old_report_agent_id);

        fixture
            .host
            .compact_agent(
                old_report_agent_id.clone(),
                AgentCompactPayload::default(),
                stream,
            )
            .await
            .expect("compact report");

        let notifies = drain_compact_notifies(&mut rx);
        let completed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Completed)
            .expect("completed notify");
        let replacement_agent_id = completed
            .new_agent_id
            .clone()
            .expect("replacement agent id");
        let replacement_session_id = completed
            .new_session_id
            .clone()
            .expect("replacement session id");
        assert_ne!(replacement_session_id, old_report_session_id);

        let snapshot = team_snapshot(&fixture.host).await;
        let report = member_from_snapshot(snapshot.clone(), &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&replacement_session_id));
        let binding = snapshot
            .bindings
            .iter()
            .find(|binding| binding.member_id == fixture.report.id)
            .expect("report binding");
        assert_eq!(
            binding.current_agent_id.as_ref(),
            Some(&replacement_agent_id)
        );

        assert!(fixture.host.close_agent(&replacement_agent_id).await);
        wait_for_team_member_unbound(&fixture.host, &fixture.report.id).await;
        let resumed = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow up after compaction".to_owned(),
                None,
            )
            .await
            .expect("message compacted report");
        let resumed_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&resumed.agent_id)
            .await
            .expect("resumed report session");
        assert_eq!(resumed_session_id, replacement_session_id);
    }

    #[tokio::test]
    async fn team_compaction_rejects_busy_team() {
        let fixture = team_fixture().await;
        let manager = fixture
            .host
            .activate_team_member(
                fixture.manager.id.clone(),
                Some(format!("{MOCK_SLOW_TURN_SENTINEL} start manager")),
                None,
            )
            .await
            .expect("activate busy manager");
        wait_for_agent_active(&fixture.host, &manager.agent_id).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let stream = Stream::new(
            StreamPath(format!("/host/team-compact-busy-{}", Uuid::new_v4())),
            tx,
        );

        let error = fixture
            .host
            .compact_team(
                TeamCompactPayload {
                    team_id: fixture.team.id.clone(),
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect_err("busy team should reject compaction");
        assert!(
            error.message.contains("not idle"),
            "unexpected error: {error}"
        );
        wait_for_agent_idle(&fixture.host, &manager.agent_id).await;
    }

    #[tokio::test]
    async fn team_compaction_rotates_live_idle_members() {
        let fixture = team_fixture().await;
        let manager = fixture
            .host
            .activate_team_member(
                fixture.manager.id.clone(),
                Some("Start the manager".to_owned()),
                None,
            )
            .await
            .expect("activate manager");
        wait_for_agent_idle(&fixture.host, &manager.agent_id).await;
        wait_for_team_member_binding_idle(&fixture.host, &fixture.manager.id).await;

        let report = fixture
            .host
            .message_team_member(
                manager.agent_id.clone(),
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");
        wait_for_agent_idle(&fixture.host, &report.agent_id).await;
        wait_for_team_member_binding_idle(&fixture.host, &fixture.report.id).await;
        let old_agent_ids = HashSet::from([manager.agent_id.clone(), report.agent_id.clone()]);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(
            StreamPath(format!("/host/team-compact-{}", Uuid::new_v4())),
            tx,
        );
        fixture
            .host
            .compact_team(
                TeamCompactPayload {
                    team_id: fixture.team.id.clone(),
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact team");

        let started = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("team compaction started notify")
            .expect("team compaction started envelope");
        assert_eq!(started.kind, FrameKind::TeamCompactNotify);
        let started: TeamCompactNotifyPayload =
            started.parse_payload().expect("team compact started");
        assert_eq!(started.status, TeamCompactStatus::Started);
        assert_eq!(started.agent_ids.len(), 2);

        let completed = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("team compaction should finish")
                .expect("team compact envelope");
            if envelope.kind != FrameKind::TeamCompactNotify {
                continue;
            }
            let payload: TeamCompactNotifyPayload =
                envelope.parse_payload().expect("team compact notify");
            if payload.status != TeamCompactStatus::Started {
                break payload;
            }
        };
        assert_eq!(completed.status, TeamCompactStatus::Completed);
        assert_eq!(completed.results.len(), 2);
        assert!(
            completed
                .results
                .iter()
                .all(|result| result.status == AgentCompactStatus::Completed)
        );
        assert!(
            completed
                .results
                .iter()
                .all(|result| old_agent_ids.contains(&result.old_agent_id))
        );

        let snapshot = team_snapshot(&fixture.host).await;
        for member_id in [&fixture.manager.id, &fixture.report.id] {
            let member = member_from_snapshot(snapshot.clone(), member_id);
            let session_id = member.session_id.expect("member session after compaction");
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            let new_agent_id = binding
                .current_agent_id
                .as_ref()
                .expect("member remains live-bound");
            assert!(!old_agent_ids.contains(new_agent_id));
            let result = completed
                .results
                .iter()
                .find(|result| result.new_agent_id.as_ref() == Some(new_agent_id))
                .expect("completed result for new agent");
            assert_eq!(result.new_session_id.as_ref(), Some(&session_id));
        }
    }

    #[tokio::test]
    async fn team_first_message_records_report_session_id() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");

        assert_eq!(outcome.member_id, fixture.report.id);
        assert!(!outcome.queued);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let session_id = report
            .session_id
            .clone()
            .expect("first spawn records session id");
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        let persisted_report = persisted
            .members
            .get(&fixture.report.id)
            .expect("persisted report");
        assert_eq!(persisted_report.session_id.as_ref(), Some(&session_id));

        let start = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .find(|start| start.agent_id == outcome.agent_id)
            .expect("spawned report agent");
        assert_eq!(start.origin, AgentOrigin::TeamMember);
        assert_eq!(start.team_id.as_ref(), Some(&fixture.team.id));
        assert_eq!(start.team_member_id.as_ref(), Some(&fixture.report.id));
    }

    #[tokio::test]
    async fn team_member_spawn_uses_union_of_project_roots() {
        let fixture = team_fixture().await;
        let second_root = fixture._dir.path().join("second-project-root");
        std::fs::create_dir_all(&second_root).expect("create second root");
        let second_root = second_root.to_string_lossy().to_string();
        fixture
            .host
            .create_project(ProjectCreatePayload {
                name: "Second Team Project".to_owned(),
                roots: vec![ProjectRootPath(second_root.clone())],
            })
            .await
            .expect("create second project");
        let second_project_id = {
            let state = fixture.host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Second Team Project")
                .expect("created second project")
                .id
        };
        fixture
            .host
            .update_team_member(TeamMemberUpdatePayload {
                id: fixture.report.id.clone(),
                name: fixture.report.name.clone(),
                description: fixture.report.description.clone(),
                profile: fixture.report.profile.clone(),
                project_ids: vec![fixture.project_id.clone(), second_project_id],
            })
            .await
            .expect("update report projects");
        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id,
                report.id.clone(),
                "Use both projects".to_owned(),
                None,
            )
            .await
            .expect("message report");

        let start = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .find(|start| start.agent_id == outcome.agent_id)
            .expect("spawned report agent");
        assert_eq!(
            start.workspace_roots,
            vec![fixture.project_root.clone(), second_root]
        );
        assert_eq!(start.project_id.as_ref(), Some(&fixture.project_id));
    }

    #[tokio::test]
    async fn team_terminal_agent_unbinds_and_resumes_next_message() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let first = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                format!("First task {MOCK_DIE_AFTER_BUSY_SENTINEL}"),
                None,
            )
            .await
            .expect("first message spawns report");
        let first_session_id =
            member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id)
                .session_id
                .expect("first session id");

        wait_for_team_member_unbound(&fixture.host, &fixture.report.id).await;

        let second = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow-up after crash".to_owned(),
                None,
            )
            .await
            .expect("second message resumes after terminal unbind");
        assert_ne!(first.agent_id, second.agent_id);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&first_session_id));
    }

    #[tokio::test]
    async fn team_subsequent_unbound_message_resumes_session() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let first = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                "First task".to_owned(),
                None,
            )
            .await
            .expect("first message");
        let first_session_id =
            member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id)
                .session_id
                .expect("first session id");

        assert!(fixture.host.close_agent(&first.agent_id).await);

        let second = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow-up task".to_owned(),
                None,
            )
            .await
            .expect("second message resumes");
        assert_ne!(first.agent_id, second.agent_id);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&first_session_id));
    }

    #[tokio::test]
    async fn team_message_member_rejects_report_caller() {
        let fixture = team_fixture().await;
        let report_agent_id = bind_team_member(&fixture.host, &fixture.report).await;

        let err = fixture
            .host
            .message_team_member(
                report_agent_id,
                fixture.report.id.clone(),
                "Try to delegate".to_owned(),
                None,
            )
            .await
            .expect_err("report caller should be rejected");
        assert!(err.starts_with("authorization:"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn team_resume_failure_marks_binding_failed() {
        let fixture = team_fixture().await;
        let report_agent_id = bind_team_member(&fixture.host, &fixture.report).await;
        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let bad_session_id = report.session_id.clone().expect("bound report session");
        {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .upsert_backend_session(
                    &BackendSession {
                        id: bad_session_id.clone(),
                        backend_kind: BackendKind::Claude,
                        workspace_roots: vec![fixture.project_root.clone()],
                        title: Some("Missing mock backend session".to_owned()),
                        token_count: None,
                        created_at_ms: Some(1),
                        updated_at_ms: Some(1),
                        resumable: true,
                    },
                    None,
                    Some(report.project_ids[0].clone()),
                    report.custom_agent_id.clone(),
                )
                .expect("persist fake session");
        }
        {
            let registry = { fixture.host.state.lock().await.team_registry.clone() };
            let events = registry
                .clear_binding_by_agent(report_agent_id)
                .await
                .expect("clear report binding");
            fixture.host.fan_out_team_registry_events(events).await;
        }
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let err = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Try resume".to_owned(),
                None,
            )
            .await
            .expect_err("unknown backend session should fail");
        assert!(
            err.contains("unknown mock session"),
            "unexpected error: {err}"
        );

        let snapshot = team_snapshot(&fixture.host).await;
        let binding = snapshot
            .bindings
            .iter()
            .find(|binding| binding.member_id == fixture.report.id)
            .expect("report binding");
        assert!(binding.current_agent_id.is_none());
        assert_eq!(binding.status, AgentControlStatus::Failed);
        let report = member_from_snapshot(snapshot, &fixture.report.id);
        assert!(report.session_id.is_none());
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        let persisted_report = persisted
            .members
            .get(&fixture.report.id)
            .expect("persisted report");
        assert!(persisted_report.session_id.is_none());
    }

    #[tokio::test]
    async fn team_delete_hard_removes_team_and_members() {
        let fixture = team_fixture().await;

        fixture
            .host
            .delete_team(TeamDeletePayload {
                id: fixture.team.id.clone(),
            })
            .await
            .expect("delete team");
        let snapshot = team_snapshot(&fixture.host).await;
        assert!(snapshot.teams.iter().all(|team| team.id != fixture.team.id));
        assert!(
            snapshot
                .members
                .iter()
                .all(|member| member.team_id != fixture.team.id)
        );
        assert!(snapshot.bindings.iter().all(|binding| {
            binding.member_id != fixture.manager.id && binding.member_id != fixture.report.id
        }));
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert!(!persisted.teams.contains_key(&fixture.team.id));
        assert!(!persisted.members.contains_key(&fixture.manager.id));
        assert!(!persisted.members.contains_key(&fixture.report.id));

        let err = fixture
            .host
            .rename_team(TeamRenamePayload {
                id: fixture.team.id.clone(),
                name: "Renamed Team".to_owned(),
            })
            .await
            .expect_err("deleted team rename should fail");
        assert_eq!(err.kind, crate::error::AppErrorKind::NotFound);
        assert!(err.message.contains("missing team"));
    }

    #[tokio::test]
    async fn concurrent_first_team_messages_spawn_at_most_one_agent() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let mut tasks = Vec::new();
        for index in 0..8 {
            let host = fixture.host.clone();
            let caller = manager_agent_id.clone();
            let member_id = fixture.report.id.clone();
            tasks.push(tokio::spawn(async move {
                host.message_team_member(
                    caller,
                    member_id,
                    format!("Concurrent task {index}"),
                    None,
                )
                .await
            }));
        }

        let mut success_count = 0;
        for task in tasks {
            match task.await.expect("message task should not panic") {
                Ok(_) => success_count += 1,
                Err(err) => assert!(
                    err.starts_with("conflict:"),
                    "unexpected concurrent message error: {err}"
                ),
            }
        }
        assert!(success_count >= 1);

        let report_agents = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .filter(|agent| agent.team_member_id.as_ref() == Some(&fixture.report.id))
            .collect::<Vec<_>>();
        assert_eq!(report_agents.len(), 1);
    }

    #[tokio::test]
    async fn team_delete_rejects_live_bound_member() {
        let fixture = team_fixture().await;
        bind_team_member(&fixture.host, &fixture.report).await;

        let err = fixture
            .host
            .delete_team_member(TeamMemberDeletePayload {
                id: fixture.report.id.clone(),
            })
            .await
            .expect_err("live-bound report should not delete");
        assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
        assert!(err.message.contains("live-bound"));
    }

    #[tokio::test]
    async fn team_references_block_custom_agent_but_project_delete_unassigns_members() {
        let fixture = team_fixture().await;

        let custom_agent_err = fixture
            .host
            .delete_custom_agent(CustomAgentDeletePayload {
                id: fixture.custom_agent_id.clone(),
            })
            .await
            .expect_err("custom agent reference should block delete");
        assert_eq!(custom_agent_err.kind, crate::error::AppErrorKind::Conflict);
        assert!(
            custom_agent_err
                .message
                .contains(r#"custom agent "Team Custom Agent""#)
        );
        assert!(custom_agent_err.message.contains(r#"team "Product Team""#));
        assert!(
            custom_agent_err
                .message
                .contains(r#"team member "Manager""#)
                || custom_agent_err.message.contains(r#"team member "Report""#)
        );
        assert!(!custom_agent_err.message.contains(&fixture.manager.id.0));
        assert!(!custom_agent_err.message.contains(&fixture.report.id.0));

        fixture
            .host
            .delete_project(ProjectDeletePayload {
                id: fixture.project_id.clone(),
            })
            .await
            .expect("project delete should detach team project refs");

        let snapshot = team_snapshot(&fixture.host).await;
        let manager = member_from_snapshot(snapshot.clone(), &fixture.manager.id);
        let report = member_from_snapshot(snapshot, &fixture.report.id);
        assert!(manager.project_ids.is_empty());
        assert!(report.project_ids.is_empty());

        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert_no_team_member_references_project(&persisted, &fixture.project_id);
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_member_and_delete_custom_agent_serialize() {
        let race_test_guard = team_mutation_race_test_lock().lock().await;
        let fixture = team_race_fixture().await;
        assert!(fixture.temp_dir.path().exists());
        let hook = install_team_mutation_after_refs_test_hook(&fixture.host, "team_member_create");
        let create_host = fixture.host.clone();
        let create_payload = team_member_create_payload(
            &fixture,
            fixture.custom_agent_id.clone(),
            vec![fixture.project_id.clone()],
        );
        let create_task =
            tokio::spawn(async move { create_host.create_team_member(create_payload).await });

        hook.wait_until_reached().await;
        let delete_host = fixture.host.clone();
        let custom_agent_id = fixture.custom_agent_id.clone();
        let delete_task = tokio::spawn(async move {
            delete_host
                .delete_custom_agent(CustomAgentDeletePayload {
                    id: custom_agent_id,
                })
                .await
        });
        tokio::task::yield_now().await;
        hook.resume();

        let create_result = create_task.await.expect("create task should not panic");
        let delete_result = delete_task.await.expect("delete task should not panic");
        match (&create_result, &delete_result) {
            (Ok(()), Err(err)) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(err.message.contains(r#"custom agent "Race Custom Agent""#));
                assert!(err.message.contains(r#"team member "Race Report""#));
            }
            (Err(err), Ok(())) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(
                    err.message.contains("references missing custom agent"),
                    "unexpected create error: {}",
                    err.message
                );
            }
            (Ok(()), Ok(())) => panic!("create and delete both succeeded"),
            (Err(create_err), Err(delete_err)) => panic!(
                "create and delete both failed: create={}, delete={}",
                create_err, delete_err
            ),
        }
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
        drop(hook);
        drop(race_test_guard);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_member_and_delete_project_serialize() {
        let race_test_guard = team_mutation_race_test_lock().lock().await;
        let fixture = team_race_fixture().await;
        assert!(fixture.temp_dir.path().exists());
        let hook = install_team_mutation_after_refs_test_hook(&fixture.host, "team_member_create");
        let create_host = fixture.host.clone();
        let create_payload = team_member_create_payload(
            &fixture,
            fixture.custom_agent_id.clone(),
            vec![fixture.project_id.clone()],
        );
        let create_task =
            tokio::spawn(async move { create_host.create_team_member(create_payload).await });

        hook.wait_until_reached().await;
        let delete_host = fixture.host.clone();
        let project_id = fixture.project_id.clone();
        let delete_task = tokio::spawn(async move {
            delete_host
                .delete_project(ProjectDeletePayload { id: project_id })
                .await
        });
        tokio::task::yield_now().await;
        hook.resume();

        let create_result = create_task.await.expect("create task should not panic");
        let delete_result = delete_task.await.expect("delete task should not panic");
        match (&create_result, &delete_result) {
            (Ok(()), Err(err)) => {
                panic!("delete should detach project refs instead of failing: {err}");
            }
            (Err(err), Ok(())) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(
                    err.message.contains("references missing project"),
                    "unexpected create error: {}",
                    err.message
                );
            }
            (Ok(()), Ok(())) => {
                let snapshot = team_snapshot(&fixture.host).await;
                let created = snapshot
                    .members
                    .iter()
                    .find(|member| member.name == "Race Report")
                    .expect("created race report");
                assert!(
                    !created.project_ids.contains(&fixture.project_id),
                    "created member retained deleted project ref"
                );
            }
            (Err(create_err), Err(delete_err)) => panic!(
                "create and delete both failed: create={}, delete={}",
                create_err, delete_err
            ),
        }
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert_no_team_member_references_project(&persisted, &fixture.project_id);
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
        drop(hook);
        drop(race_test_guard);
    }

    #[tokio::test]
    async fn ai_reviewer_backend_resolution_uses_host_defaults() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_store_paths_and_runtime_config(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
            HostRuntimeConfig::default(),
        )
        .expect("spawn host");

        let err = host
            .resolve_ai_reviewer_backend_kind(None)
            .await
            .expect_err("missing host backend should fail");
        assert!(
            err.contains("no default_backend or enabled backends"),
            "unexpected error: {err}"
        );

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Codex, BackendKind::Claude],
            },
        })
        .await
        .expect("enable backends");
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(None)
                .await
                .expect("first enabled backend"),
            BackendKind::Claude
        );

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Codex),
            },
        })
        .await
        .expect("set default backend");
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(None)
                .await
                .expect("default backend"),
            BackendKind::Codex
        );
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(Some(BackendKind::Antigravity))
                .await
                .expect("explicit backend override"),
            BackendKind::Antigravity
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ai_reviewer_non_claude_reaches_read_only_spawn_preconditions() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_store_paths_and_runtime_config(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
            HostRuntimeConfig::default(),
        )
        .expect("spawn host");
        host.state.lock().await.review_mcp.url.clear();

        let (review_tx, _review_rx) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        let project_store = {
            let state = host.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .create(
                "Review Project".to_owned(),
                vec![ProjectRootPath("/tmp/review-root".to_owned())],
            )
            .expect("create review project");
        let review = Review {
            id: ReviewId("review-test".to_string()),
            project_id: project.id,
            origin_agent_id: AgentId("agent-test".to_string()),
            origin_session_id: SessionId("session-test".to_string()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/tmp/review-root".to_string()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::Hunks,
                files: vec![ProjectGitDiffFile {
                    relative_path: "src/lib.rs".to_owned(),
                    is_binary: false,
                    hunks: Vec::new(),
                }],
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        };

        let (_reply, result) = host
            .spawn_ai_reviewer(crate::review::actor::ReviewAiSpawnRequest {
                review_id: review.id.clone(),
                review,
                backend_kind: Some(BackendKind::Codex),
                cost_hint: None,
                instructions: None,
                review_handle: ReviewHandle { tx: review_tx },
                reply,
            })
            .await;

        let err = result.expect_err("missing MCP should fail before spawning");
        assert!(
            err.contains("review feedback MCP server is unavailable"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
