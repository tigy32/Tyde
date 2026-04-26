use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use protocol::types::AgentClosedPayload;
use protocol::{
    AgentId, AgentOrigin, AgentStartPayload, BackendSetupPayload, CustomAgentDeletePayload,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, FrameKind, HostBrowseListPayload,
    HostBrowseStartPayload, HostSettingsPayload, McpServerDeletePayload, McpServerNotifyPayload,
    McpServerUpsertPayload, NewAgentPayload, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDiscardFilePayload, ProjectGitCommitPayload,
    ProjectGitCommitResultPayload, ProjectId, ProjectListDirPayload, ProjectNotifyPayload,
    ProjectPath, ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectRootPath, ProjectStageFilePayload, ProjectStageHunkPayload,
    ProjectUnstageFilePayload, RunBackendSetupPayload, SessionId, SessionListPayload,
    SessionSchemaEntry, SessionSchemasPayload, SessionSettingsSchema, SetSettingPayload,
    SkillNotifyPayload, SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload,
    SteeringDeletePayload, SteeringNotifyPayload, SteeringScope, SteeringUpsertPayload, StreamPath,
    TerminalCreatePayload, TerminalId, TerminalLaunchTarget, TerminalResizePayload,
    TerminalSendPayload,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::agent::customization::{
    ResolveSpawnConfigRequest, ResolvedSpawnConfig, protocol_mcp_servers_to_startup,
    resolve_spawn_config,
};
use crate::agent::registry::{
    AgentRegistry, InitialAgentAlias, InitialAgentAliasPersistence, RelaySpawnRequest,
    ResolvedSpawnRequest,
};
use crate::agent::{AgentHandle, GenerateAgentNameRequest, derive_agent_name, generate_agent_name};
use crate::agent_control_mcp::AgentControlMcpHandle;
use crate::backend::setup;
use crate::backend::{
    BackendSession, StartupMcpServer, StartupMcpTransport, sanitize_session_settings_values,
    session_settings_schema_for_backend, validate_session_settings_values,
};
use crate::browse_stream;
use crate::debug_mcp::DebugMcpHandle;
use crate::error::{AppError, AppResult};
use crate::project_stream::{
    ProjectDiffRequestKey, ProjectStreamHandle, ProjectStreamSubscription, build_dir_listing,
    commit, discard_file, read_diff, read_file, spawn_project_subscription, stage_file, stage_hunk,
    unstage_file,
};
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::McpServerStore;
use crate::store::project::ProjectStore;
use crate::store::session::SessionStore;
use crate::store::settings::HostSettingsStore;
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;
use crate::stream::{Stream, StreamClosed};
use crate::sub_agent::{
    HostSubAgentSpawnRequest, HostSubAgentSpawnRx, HostSubAgentSpawnTx, SubAgentEmitter,
    SubAgentHandle,
};
use crate::terminal_stream::{TerminalHandle, TerminalLaunchInfo, create_terminal};

struct HostSubscriber {
    stream: Stream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildCompletionOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct ChildCompletionNotice {
    pub parent_id: AgentId,
    pub child_id: AgentId,
    pub child_name: String,
    pub outcome: ChildCompletionOutcome,
    pub message_text: String,
}

pub(crate) type HostChildCompletionNoticeTx = mpsc::UnboundedSender<ChildCompletionNotice>;
pub(crate) type HostChildCompletionNoticeRx = mpsc::UnboundedReceiver<ChildCompletionNotice>;

#[derive(Clone, Debug, Default)]
pub struct HostRuntimeConfig {
    pub debug_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub agent_control_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub kiro_probe_program: Option<String>,
}

#[derive(Clone, Debug)]
enum KiroSessionSchemaState {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

pub(crate) struct HostState {
    pub registry: AgentRegistry,
    pub project_store: Arc<Mutex<ProjectStore>>,
    pub settings_store: Arc<Mutex<HostSettingsStore>>,
    pub session_store: Arc<Mutex<SessionStore>>,
    pub custom_agent_store: Arc<Mutex<CustomAgentStore>>,
    pub mcp_server_store: Arc<Mutex<McpServerStore>>,
    pub steering_store: Arc<Mutex<SteeringStore>>,
    pub skill_store: Arc<Mutex<SkillStore>>,
    pub agent_sessions: HashMap<AgentId, SessionId>,
    pub sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub child_completion_tx: HostChildCompletionNoticeTx,
    pub use_mock_backend: bool,
    pub debug_mcp: DebugMcpHandle,
    pub agent_control_mcp: AgentControlMcpHandle,
    kiro_session_schema: KiroSessionSchemaState,
    kiro_probe_program: Option<String>,
    host_streams: HashMap<StreamPath, HostSubscriber>,
    project_streams: HashMap<ProjectId, ProjectStreamSubscription>,
    terminal_streams: HashMap<(StreamPath, TerminalId), TerminalHandle>,
    browse_streams: HashMap<(StreamPath, StreamPath), Stream>,
}

#[derive(Clone)]
pub struct HostHandle {
    state: Arc<Mutex<HostState>>,
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
    settings: PathBuf,
    custom_agent: PathBuf,
    mcp_server: PathBuf,
    steering: PathBuf,
    skills_index: PathBuf,
    skills_root_dir: PathBuf,
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
    pub(crate) async fn register_host_stream(&self, host_stream: Stream) {
        let backend_setup = setup::collect_backend_setup().await;
        let mut state = self.state.lock().await;
        let host_path = host_stream.path().clone();

        let previous = state.host_streams.insert(
            host_path.clone(),
            HostSubscriber {
                stream: host_stream,
            },
        );
        assert!(
            previous.is_none(),
            "duplicate host stream registration for {}",
            host_path
        );

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
        let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
            panic!(
                "host stream {} disappeared during settings replay",
                host_path
            );
        };
        if emit_host_settings_for_subscriber(&settings, subscriber)
            .await
            .is_err()
        {
            state.host_streams.remove(&host_path);
            return;
        }
        if emit_session_schemas_for_subscriber(&schemas, subscriber)
            .await
            .is_err()
        {
            state.host_streams.remove(&host_path);
            return;
        }
        if emit_backend_setup_for_subscriber(&backend_setup, subscriber)
            .await
            .is_err()
        {
            state.host_streams.remove(&host_path);
            return;
        }

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
        for project in projects {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during project registration replay",
                    host_path
                );
            };
            if emit_project_notify_for_subscriber(
                &ProjectNotifyPayload::Upsert { project },
                subscriber,
            )
            .await
            .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }
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

        let mcp_servers = state
            .mcp_server_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list MCP servers for host registration: {err}")
            });
        for mcp_server in mcp_servers {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during MCP server registration replay",
                    host_path
                );
            };
            if emit_mcp_server_notify_for_subscriber(
                &McpServerNotifyPayload::Upsert { mcp_server },
                subscriber,
            )
            .await
            .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }

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
        for skill in skills {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during skill registration replay",
                    host_path
                );
            };
            if emit_skill_notify_for_subscriber(&SkillNotifyPayload::Upsert { skill }, subscriber)
                .await
                .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }

        let steering = state
            .steering_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list steering for host registration: {err}"));
        for steering in steering {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during steering registration replay",
                    host_path
                );
            };
            if emit_steering_notify_for_subscriber(
                &SteeringNotifyPayload::Upsert { steering },
                subscriber,
            )
            .await
            .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }

        let custom_agents = state
            .custom_agent_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list custom agents for host registration: {err}")
            });
        for custom_agent in custom_agents {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during custom agent registration replay",
                    host_path
                );
            };
            if emit_custom_agent_notify_for_subscriber(
                &CustomAgentNotifyPayload::Upsert { custom_agent },
                subscriber,
            )
            .await
            .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }

        let agent_ids = state.registry.agent_ids();
        for agent_id in agent_ids {
            let agent_handle = state.registry.agent_handle(&agent_id).unwrap_or_else(|| {
                panic!(
                    "registry missing handle for listed agent {} during host stream registration",
                    agent_id
                )
            });
            let start = agent_handle.snapshot().await.unwrap_or_else(|| {
                panic!(
                    "agent {} disappeared while replaying host registration",
                    agent_id
                )
            });
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during registration replay",
                    host_path
                );
            };
            if emit_new_agent_for_stream(&start, &agent_handle, &subscriber.stream)
                .await
                .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }

        drop(state);
        if refresh_kiro_schema {
            self.schedule_session_schema_refresh();
        }
    }

    pub(crate) async fn unregister_host_stream(&self, path: &StreamPath) {
        let (project_handles, terminals) = {
            let mut state = self.state.lock().await;
            state.host_streams.remove(path);
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
            (project_handles, terminals)
        };

        for handle in project_handles {
            handle.remove_subscriber(path.clone()).await;
        }

        for terminal in terminals {
            terminal.close().await;
        }
    }

    pub(crate) async fn spawn_agent(&self, payload: SpawnAgentPayload) -> AgentId {
        self.spawn_agent_with_origin(payload, AgentOrigin::User)
            .await
    }

    async fn spawn_agent_with_origin(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
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
                payload
                    .parent_agent_id
                    .as_ref()
                    .and_then(|agent_id| state.agent_sessions.get(agent_id).cloned()),
            )
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
                session_settings,
            } => {
                if let Some(project_id) = &payload.project_id {
                    project_store
                        .lock()
                        .await
                        .get(project_id)
                        .unwrap_or_else(|| {
                            panic!("cannot spawn agent in missing project {}", project_id)
                        });
                }
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                );
                let requested_custom_agent_id = payload.custom_agent_id.clone();
                let (
                    effective_custom_agent_id,
                    resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind,
                        project_id: payload.project_id.as_ref(),
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
                            Some(err),
                        ),
                    }
                };
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
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id: payload.project_id,
                    backend_kind,
                    workspace_roots,
                    initial_input: Some(protocol::SendMessagePayload {
                        message: prompt,
                        images,
                    }),
                    cost_hint,
                    session_settings,
                    session_settings_schema,
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: None,
                    startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Resume { session_id, prompt } => {
                let record = session_store
                    .lock()
                    .await
                    .get(&session_id)
                    .unwrap_or_else(|| panic!("cannot resume missing session {}", session_id));
                assert!(
                    record.resumable,
                    "cannot resume non-resumable session {}",
                    session_id
                );
                if let Some(requested_custom_agent_id) = payload.custom_agent_id.as_ref() {
                    assert_eq!(
                        record.custom_agent_id.as_ref(),
                        Some(requested_custom_agent_id),
                        "resume custom_agent_id {:?} must match stored session custom_agent_id {:?}",
                        requested_custom_agent_id,
                        record.custom_agent_id
                    );
                }
                let project_id = payload.project_id.or(record.project_id.clone());
                if let Some(project_id) = &project_id {
                    project_store
                        .lock()
                        .await
                        .get(project_id)
                        .unwrap_or_else(|| {
                            panic!("cannot resume agent in missing project {}", project_id)
                        });
                }
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &record.workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
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
                                Some(err),
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
                        Err(err) => (None, ResolvedSpawnConfig::default(), None, Some(err)),
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
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin,
                    custom_agent_id: effective_custom_agent_id,
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id,
                    backend_kind: record.backend_kind,
                    workspace_roots: record.workspace_roots,
                    initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                        message: prompt,
                        images: None,
                    }),
                    cost_hint: None,
                    session_settings: sanitized_settings,
                    session_settings_schema,
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: Some(session_id),
                    startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
        };

        tracing::info!(
            backend_kind = ?request.backend_kind,
            workspace_roots = ?request.workspace_roots,
            startup_mcp_servers = request.startup_mcp_servers.len(),
            resume_session_id = ?request.resume_session_id,
            "host spawn_agent resolved request"
        );

        let (start, agent_handle, startup_rx, host_streams) = {
            let mut state = self.state.lock().await;
            let sub_agent_spawn_tx = state.sub_agent_spawn_tx.clone();
            let child_completion_tx = state.child_completion_tx.clone();
            let spawned = state.registry.spawn(
                request,
                Arc::clone(&session_store),
                sub_agent_spawn_tx,
                child_completion_tx,
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

    pub(crate) async fn create_project(&self, payload: ProjectCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "project_create";
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .create(payload.name, payload.roots)
            .map_err(|error| project_store_error(OPERATION, error))?;
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
        }
        Ok(())
    }

    pub(crate) async fn rename_project(&self, payload: ProjectRenamePayload) -> AppResult<()> {
        const OPERATION: &str = "project_rename";
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .rename(&payload.id, payload.name)
            .map_err(|error| project_store_error(OPERATION, error))?;
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        Ok(())
    }

    pub(crate) async fn reorder_projects(&self, payload: ProjectReorderPayload) -> AppResult<()> {
        const OPERATION: &str = "project_reorder";
        let mut state = self.state.lock().await;
        let projects = state
            .project_store
            .lock()
            .await
            .reorder(payload.project_ids)
            .map_err(|error| project_store_error(OPERATION, error))?;
        for project in projects {
            fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        }
        Ok(())
    }

    pub(crate) async fn add_project_root(&self, payload: ProjectAddRootPayload) -> AppResult<()> {
        const OPERATION: &str = "project_add_root";
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .add_root(&payload.id, payload.root)
            .map_err(|error| project_store_error(OPERATION, error))?;
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

    pub(crate) async fn delete_project(&self, payload: ProjectDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "project_delete";
        let mut state = self.state.lock().await;
        let referenced_session = state
            .session_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|record| record.project_id.as_ref() == Some(&payload.id));
        if let Some(session) = referenced_session {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete project {} while referenced by session {}",
                    payload.id, session.id
                ),
            ));
        }
        let referenced_steering = state
            .steering_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|steering| matches!(&steering.scope, SteeringScope::Project(project_id) if project_id == &payload.id));
        if let Some(steering) = referenced_steering {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete project {} while referenced by steering {}",
                    payload.id, steering.id
                ),
            ));
        }
        let project = state
            .project_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| project_store_error(OPERATION, error))?;
        if let Some(subscription) = state.project_streams.remove(&payload.id) {
            subscription.task.abort();
        }
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Delete { project }).await;
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

    pub(crate) async fn upsert_mcp_server(&self, payload: McpServerUpsertPayload) -> AppResult<()> {
        const OPERATION: &str = "mcp_server_upsert";
        let mut state = self.state.lock().await;
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

    pub(crate) async fn list_sessions(&self, host_output_stream: &Stream) -> AppResult<()> {
        const OPERATION: &str = "list_sessions";
        let sessions = {
            let state = self.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .summaries()
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
        };

        let payload = SessionListPayload { sessions };
        let payload = serde_json::to_value(&payload).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize SessionList payload for host stream",
                error,
            )
        })?;
        let _ = host_output_stream
            .send_value(FrameKind::SessionList, payload)
            .await;
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
            payload.setting,
            protocol::HostSettingValue::EnabledBackends { .. }
        );
        let settings = state
            .settings_store
            .lock()
            .await
            .apply(payload.setting)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        fan_out_host_settings(&mut state, settings).await;
        if refresh_session_schemas {
            drop(state);
            self.refresh_session_schemas().await;
        }
        Ok(())
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
        let terminal_id = TerminalId(Uuid::new_v4().to_string());
        let terminal_stream_path = StreamPath(format!("/terminal/{}", terminal_id));
        let terminal_output_stream = host_output_stream.with_path(terminal_stream_path.clone());
        let terminal = create_terminal(launch, terminal_output_stream)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;

        {
            let mut state = self.state.lock().await;
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
            .await
            .is_err()
        {
            return Ok(None);
        }
        let _ = terminal.emit_start().await;
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

    pub(crate) async fn interrupt_agent(&self, agent_id: &AgentId) -> bool {
        let (parent_handle, candidate_handles) = {
            let state = self.state.lock().await;
            let Some(parent_handle) = state.registry.agent_handle(agent_id) else {
                return false;
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
            let Some(start) = handle.snapshot().await else {
                continue;
            };
            if start.parent_agent_id.as_ref() == Some(agent_id)
                && start.origin != AgentOrigin::BackendNative
            {
                tyde_owned_children.push(handle);
            }
        }

        let interrupted = parent_handle.interrupt().await;
        for child in tyde_owned_children {
            let _ = child.interrupt().await;
        }

        interrupted
    }

    pub(crate) async fn close_agent(&self, agent_id: &AgentId) -> bool {
        let (agent_handle, host_streams) = {
            let state = self.state.lock().await;
            let Some(agent_handle) = state.registry.agent_handle(agent_id) else {
                return false;
            };
            let host_streams = state
                .host_streams
                .iter()
                .map(|(path, subscriber)| (path.clone(), subscriber.stream.clone()))
                .collect::<Vec<_>>();
            (agent_handle, host_streams)
        };

        let _ = agent_handle.close().await;

        let payload = AgentClosedPayload {
            agent_id: agent_id.clone(),
        };
        let mut dead_paths = Vec::new();
        for (path, stream) in host_streams {
            if emit_agent_closed_for_stream(&payload, &stream)
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

        let mut state = self.state.lock().await;
        let removed = state.registry.remove_agent(agent_id);
        assert!(
            removed.is_some(),
            "agent {} disappeared from registry before close completed",
            agent_id
        );
        state.agent_sessions.remove(agent_id);

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
            if let Some(start) = handle.snapshot().await {
                starts.push(start);
            }
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

        let parent_start = parent_handle.snapshot().await.unwrap_or_else(|| {
            panic!(
                "parent agent {} disappeared before backend-native child spawn",
                request.parent_agent_id
            )
        });
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

        let _ = start;
        SubAgentHandle { event_tx }
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

        let initial = payload.initial.unwrap_or_else(browse_stream::home_dir);
        let opened = browse_stream::opened_payload(&initial);
        browse_stream::emit_opened(&browse_stream, &opened).await;

        match browse_stream::list_dir(&initial, payload.include_hidden).await {
            Ok(entries) => browse_stream::emit_entries(&browse_stream, &entries).await,
            Err(error) => browse_stream::emit_error(&browse_stream, &error).await,
        }
        Ok(())
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
        let handle = ensure_project_actor(&mut state, project_id)
            .await
            .map_err(|error| project_command_error(operation, error))?;
        handle
            .add_subscriber(
                connection_host_stream.clone(),
                project_output_stream.clone(),
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
        let _ = project_output_stream
            .send_value(FrameKind::ProjectFileContents, payload)
            .await;
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
        let _ = project_output_stream
            .send_value(FrameKind::ProjectFileList, payload)
            .await;
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
        let _ = project_output_stream
            .send_value(FrameKind::ProjectGitDiff, payload)
            .await;
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
        let _ = project_output_stream
            .send_value(FrameKind::ProjectGitCommitResult, result_payload)
            .await;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            None,
        )
        .await?;
        Ok(())
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
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| AppError::not_found(operation, format!("project {} not found", project_id)))
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
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?;
    Ok(projects
        .into_iter()
        .any(|project| project.id == *project_id))
}

fn project_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing project") {
        AppError::not_found(operation, error)
    } else if error.contains("already contains root") || error.contains("duplicate id") {
        AppError::conflict(operation, error)
    } else {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
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
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            settings: settings_path,
            custom_agent: custom_agent_path,
            mcp_server: mcp_server_path,
            steering: steering_path,
            skills_index: skills_index_path,
            skills_root_dir,
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
            settings: settings_path,
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
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
            settings: settings_path,
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
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
    let session_store = SessionStore::load(paths.session)?;
    let project_store = ProjectStore::load(paths.project)?;
    let settings_store = HostSettingsStore::load(paths.settings)?;
    let custom_agent_store = CustomAgentStore::load(paths.custom_agent)?;
    let mcp_server_store = McpServerStore::load(paths.mcp_server)?;
    let steering_store = SteeringStore::load(paths.steering)?;
    let skill_store = SkillStore::load(paths.skills_index, paths.skills_root_dir)?;
    let (sub_agent_spawn_tx, sub_agent_spawn_rx) =
        mpsc::unbounded_channel::<HostSubAgentSpawnRequest>();
    let (child_completion_tx, child_completion_rx) =
        mpsc::unbounded_channel::<ChildCompletionNotice>();
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
    let host = HostHandle {
        state: Arc::new(Mutex::new(HostState {
            registry: AgentRegistry::new(),
            project_store: Arc::new(Mutex::new(project_store)),
            settings_store: Arc::new(Mutex::new(settings_store)),
            session_store: Arc::new(Mutex::new(session_store)),
            custom_agent_store: Arc::new(Mutex::new(custom_agent_store)),
            mcp_server_store: Arc::new(Mutex::new(mcp_server_store)),
            steering_store: Arc::new(Mutex::new(steering_store)),
            skill_store: Arc::new(Mutex::new(skill_store)),
            agent_sessions: HashMap::new(),
            sub_agent_spawn_tx,
            child_completion_tx,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp: agent_control_mcp_placeholder,
            kiro_session_schema: KiroSessionSchemaState::Pending,
            kiro_probe_program: runtime_config.kiro_probe_program.clone(),
            host_streams: HashMap::new(),
            project_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
            browse_streams: HashMap::new(),
        })),
    };

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

    spawn_host_sub_agent_task(host.clone(), sub_agent_spawn_rx);
    spawn_child_completion_notice_task(host.clone(), child_completion_rx);

    Ok(host)
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

fn spawn_child_completion_notice_task(host: HostHandle, mut rx: HostChildCompletionNoticeRx) {
    let worker = async move {
        while let Some(notice) = rx.recv().await {
            process_child_completion_notice(&host, notice).await;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-child-completions".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build child completion runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn child completion worker thread");
}

async fn process_child_completion_notice(host: &HostHandle, mut notice: ChildCompletionNotice) {
    let (parent_handle, parent_status, parent_start, child_start) = {
        let state = host.state.lock().await;
        (
            state.registry.agent_handle(&notice.parent_id),
            state.registry.agent_status_handle(&notice.parent_id),
            state.registry.agent_handle(&notice.parent_id),
            state.registry.agent_handle(&notice.child_id),
        )
    };

    let Some(parent_handle) = parent_handle else {
        tracing::warn!(
            parent_agent_id = %notice.parent_id,
            child_agent_id = %notice.child_id,
            "dropping child completion notice because parent agent no longer exists"
        );
        return;
    };

    let Some(parent_status_handle) = parent_status else {
        tracing::warn!(
            parent_agent_id = %notice.parent_id,
            child_agent_id = %notice.child_id,
            "dropping child completion notice because parent status is missing"
        );
        return;
    };
    let parent_status = parent_status_handle.snapshot().await;
    if parent_status.terminated {
        tracing::warn!(
            parent_agent_id = %notice.parent_id,
            child_agent_id = %notice.child_id,
            "dropping child completion notice because parent agent is terminated"
        );
        return;
    }

    if let Some(parent_start_handle) = parent_start
        && let Some(parent_snapshot) = parent_start_handle.snapshot().await
        && parent_snapshot.origin == AgentOrigin::BackendNative
    {
        tracing::warn!(
            parent_agent_id = %notice.parent_id,
            child_agent_id = %notice.child_id,
            "dropping child completion notice because backend-native relay parents do not accept auto follow-ups"
        );
        return;
    }

    if notice.child_name.trim().is_empty()
        && let Some(child_handle) = child_start
        && let Some(child_snapshot) = child_handle.snapshot().await
    {
        notice.child_name = child_snapshot.name;
    }

    if notice.child_name.trim().is_empty() {
        notice.child_name = "unknown-child".to_string();
    }

    if !parent_handle
        .enqueue_auto_follow_up(format_child_completion_notice(&notice))
        .await
    {
        tracing::warn!(
            parent_agent_id = %notice.parent_id,
            child_agent_id = %notice.child_id,
            "dropping child completion notice because parent agent is no longer accepting input"
        );
    }
}

fn format_child_completion_notice(notice: &ChildCompletionNotice) -> String {
    let outcome = match notice.outcome {
        ChildCompletionOutcome::Completed => "completed",
        ChildCompletionOutcome::Cancelled => "cancelled",
        ChildCompletionOutcome::Failed => "failed",
    };

    format!(
        "[TYDE CHILD AGENT UPDATE]\nThis is an automatic system-generated child completion notice, not a user instruction.\nChild name: {}\nChild id: {}\nChild state: idle\nChild outcome: {}\n\nChild message:\n{}\n[END TYDE CHILD AGENT UPDATE]",
        notice.child_name, notice.child_id, outcome, notice.message_text,
    )
}

pub(crate) fn startup_mcp_servers_for_settings(
    settings: &protocol::HostSettings,
    workspace_roots: &[String],
    debug_mcp: &DebugMcpHandle,
    agent_control_mcp: &AgentControlMcpHandle,
) -> Vec<StartupMcpServer> {
    let mut servers = Vec::new();

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

fn debug_mcp_url_for_repo_root(base_url: &str, repo_root: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{separator}repo_root={}",
        percent_encode_query_component(repo_root)
    )
}

pub(crate) fn agent_control_mcp_url_for_agent(base_url: &str, agent_id: &AgentId) -> String {
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
        project_id: start.project_id.clone(),
        parent_agent_id: start.parent_agent_id.clone(),
        created_at_ms: start.created_at_ms,
        instance_stream: instance_stream.clone(),
    };

    let payload = serde_json::to_value(&new_agent)
        .expect("failed to serialize NewAgent payload for host stream fanout");
    stream.send_value(FrameKind::NewAgent, payload).await?;

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
    let sessions = state
        .session_store
        .lock()
        .await
        .summaries()
        .unwrap_or_else(|err| panic!("failed to list sessions for fanout: {err}"));
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

    let subscription =
        spawn_project_subscription(Arc::clone(&state.project_store), project_id.clone()).await?;
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
    let handle = ensure_project_actor(state, project_id).await?;
    handle
        .add_subscriber(host_path.clone(), project_output_stream)
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

async fn emit_project_notify_for_subscriber(
    payload: &ProjectNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize ProjectNotify payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::ProjectNotify, payload)
        .await
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
        .await
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
        .await
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
        .await
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
        .await
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
        .await
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
        .await
}

async fn emit_agent_closed_for_stream(
    payload: &AgentClosedPayload,
    stream: &Stream,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize AgentClosed payload for host stream fanout");
    stream.send_value(FrameKind::AgentClosed, payload).await
}

async fn emit_session_schemas_for_subscriber(
    schemas: &[SessionSchemaEntry],
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(SessionSchemasPayload {
        schemas: schemas.to_vec(),
    })
    .expect("failed to serialize SessionSchemas payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::SessionSchemas, payload)
        .await
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
    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory".to_string())?;
    Ok(home)
}

fn new_instance_stream(agent_id: &AgentId) -> StreamPath {
    let instance_id = Uuid::new_v4();
    StreamPath(format!("/agent/{}/{}", agent_id, instance_id))
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
            let roots = project.roots.iter().cloned().collect::<HashSet<_>>();
            if !roots.contains(&root.0) {
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
}
