use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use protocol::{
    AgentId, AgentStartPayload, FrameKind, HostSettingsPayload, NewAgentPayload,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectFileListPayload,
    ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload, ProjectPath,
    ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRefreshPayload, ProjectRenamePayload,
    ProjectRootPath, ProjectStageFilePayload, ProjectStageHunkPayload, SessionId,
    SessionListPayload, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
    TerminalCreatePayload, TerminalId, TerminalLaunchTarget, TerminalResizePayload,
    TerminalSendPayload,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::agent::AgentHandle;
use crate::agent::registry::{AgentRegistry, ResolvedSpawnRequest};
use crate::backend::BackendSession;
use crate::project_stream::{
    ProjectSnapshotState, ProjectStreamSubscription, build_file_list, build_git_status, read_diff,
    read_file, spawn_project_subscription, stage_file, stage_hunk, sync_snapshot_state,
};
use crate::store::project::ProjectStore;
use crate::store::settings::HostSettingsStore;
use crate::store::session::SessionStore;
use crate::stream::{Stream, StreamClosed};
use crate::terminal_stream::{TerminalHandle, TerminalLaunchInfo, create_terminal};

struct HostSubscriber {
    stream: Stream,
}

pub(crate) struct HostState {
    pub registry: AgentRegistry,
    pub project_store: Arc<Mutex<ProjectStore>>,
    pub settings_store: Arc<Mutex<HostSettingsStore>>,
    pub session_store: Arc<Mutex<SessionStore>>,
    pub agent_sessions: HashMap<AgentId, SessionId>,
    pub use_mock_backend: bool,
    host_streams: HashMap<StreamPath, HostSubscriber>,
    project_streams: HashMap<(StreamPath, StreamPath), ProjectStreamSubscription>,
    terminal_streams: HashMap<(StreamPath, TerminalId), TerminalHandle>,
}

#[derive(Clone)]
pub struct HostHandle {
    state: Arc<Mutex<HostState>>,
}

impl HostHandle {
    pub(crate) async fn register_host_stream(&self, host_stream: Stream) {
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

        let projects = state
            .project_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list projects for host registration: {err}"));
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

        let starts = state.registry.list_agents();
        for start in starts {
            let agent_handle = state
                .registry
                .agent_handle(&start.agent_id)
                .unwrap_or_else(|| {
                    panic!(
                        "registry missing handle for listed agent {} during host stream registration",
                        start.agent_id
                    )
                });
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during registration replay",
                    host_path
                );
            };
            if emit_new_agent_for_subscriber(&start, &agent_handle, subscriber)
                .await
                .is_err()
            {
                state.host_streams.remove(&host_path);
                return;
            }
        }
    }

    pub(crate) async fn unregister_host_stream(&self, path: &StreamPath) {
        let terminals = {
            let mut state = self.state.lock().await;
            state.host_streams.remove(path);
            let project_stream_keys = state
                .project_streams
                .keys()
                .filter(|(host_stream, _)| host_stream == path)
                .cloned()
                .collect::<Vec<_>>();
            for key in project_stream_keys {
                let Some(subscription) = state.project_streams.remove(&key) else {
                    continue;
                };
                subscription.task.abort();
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
            terminals
        };

        for terminal in terminals {
            terminal.close().await;
        }
    }

    pub(crate) async fn spawn_agent(&self, payload: SpawnAgentPayload) {
        let mut state = self.state.lock().await;
        let session_store = Arc::clone(&state.session_store);
        let project_store = Arc::clone(&state.project_store);
        let use_mock_backend = state.use_mock_backend;
        let parent_session_id = payload
            .parent_agent_id
            .as_ref()
            .and_then(|agent_id| state.agent_sessions.get(agent_id).cloned());

        let request = match payload.params {
            SpawnAgentParams::New {
                workspace_roots,
                prompt,
                backend_kind,
                cost_hint,
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
                assert!(
                    !workspace_roots.is_empty(),
                    "spawn_agent requires at least one workspace root"
                );
                ResolvedSpawnRequest {
                    name: payload.name,
                    parent_agent_id: payload.parent_agent_id,
                    project_id: payload.project_id,
                    backend_kind,
                    workspace_roots,
                    initial_prompt: prompt,
                    cost_hint,
                    resume_session_id: None,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Resume { session_id, prompt } => {
                let record = session_store
                    .lock()
                    .await
                    .get(&session_id)
                    .unwrap_or_else(|| panic!("cannot resume missing session {}", session_id));
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
                ResolvedSpawnRequest {
                    name: payload.name,
                    parent_agent_id: payload.parent_agent_id,
                    project_id,
                    backend_kind: record.backend_kind,
                    workspace_roots: record.workspace_roots,
                    initial_prompt: prompt,
                    cost_hint: None,
                    resume_session_id: Some(session_id),
                    use_mock_backend,
                }
            }
        };

        let (start, session_id) = state
            .registry
            .spawn(request, Arc::clone(&session_store))
            .await
            .unwrap_or_else(|err| panic!("failed to spawn agent backend: {err}"));

        let session = BackendSession {
            id: session_id.clone(),
            backend_kind: start.backend_kind,
            workspace_roots: start.workspace_roots.clone(),
            title: Some(start.name.clone()),
            token_count: None,
            created_at_ms: Some(start.created_at_ms),
            updated_at_ms: Some(start.created_at_ms),
            resumable: true,
        };
        state
            .session_store
            .lock()
            .await
            .upsert_backend_session(&session, parent_session_id, start.project_id.clone())
            .unwrap_or_else(|err| panic!("failed to upsert session {}: {err}", session.id));

        state
            .agent_sessions
            .insert(start.agent_id.clone(), session_id.clone());

        let agent_handle = state
            .registry
            .agent_handle(&start.agent_id)
            .unwrap_or_else(|| {
                panic!(
                    "registry missing handle for newly spawned agent {}",
                    start.agent_id
                )
            });

        let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
        let mut dead_paths = Vec::new();

        for path in paths {
            let Some(subscriber) = state.host_streams.get_mut(&path) else {
                continue;
            };
            if emit_new_agent_for_subscriber(&start, &agent_handle, subscriber)
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

    pub(crate) async fn create_project(&self, payload: ProjectCreatePayload) {
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .create(payload.name, payload.roots)
            .unwrap_or_else(|err| panic!("failed to create project: {err}"));
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
    }

    pub(crate) async fn rename_project(&self, payload: ProjectRenamePayload) {
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .rename(&payload.id, payload.name)
            .unwrap_or_else(|err| panic!("failed to rename project {}: {err}", payload.id));
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
    }

    pub(crate) async fn add_project_root(&self, payload: ProjectAddRootPayload) {
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .add_root(&payload.id, payload.root)
            .unwrap_or_else(|err| panic!("failed to add root to project {}: {err}", payload.id));
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
    }

    pub(crate) async fn delete_project(&self, payload: ProjectDeletePayload) {
        let mut state = self.state.lock().await;
        let sessions = state
            .session_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list sessions before project delete: {err}"));
        if let Some(session) = sessions
            .iter()
            .find(|session| session.project_id.as_ref() == Some(&payload.id))
        {
            panic!(
                "cannot delete project {} because session {} still references it",
                payload.id, session.id
            );
        }

        let project = state
            .project_store
            .lock()
            .await
            .delete(&payload.id)
            .unwrap_or_else(|err| panic!("failed to delete project {}: {err}", payload.id));
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Delete { project }).await;
    }

    pub(crate) async fn list_sessions(&self, host_output_stream: &Stream) {
        let sessions = {
            let state = self.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .summaries()
                .unwrap_or_else(|err| panic!("failed to list sessions: {err}"))
        };

        let payload = SessionListPayload { sessions };
        let payload = serde_json::to_value(&payload)
            .expect("failed to serialize SessionList payload for host stream");
        let _ = host_output_stream
            .send_value(FrameKind::SessionList, payload)
            .await;
    }

    pub(crate) async fn dump_settings(&self, host_output_stream: &Stream) {
        let settings = {
            let state = self.state.lock().await;
            state
                .settings_store
                .lock()
                .await
                .get()
                .unwrap_or_else(|err| panic!("failed to load host settings: {err}"))
        };

        let payload = serde_json::to_value(HostSettingsPayload { settings })
            .expect("failed to serialize HostSettings payload for host stream");
        let _ = host_output_stream
            .send_value(FrameKind::HostSettings, payload)
            .await;
    }

    pub(crate) async fn set_setting(&self, payload: SetSettingPayload) {
        let mut state = self.state.lock().await;
        let settings = state
            .settings_store
            .lock()
            .await
            .apply(payload.setting)
            .unwrap_or_else(|err| panic!("failed to apply host setting: {err}"));
        fan_out_host_settings(&mut state, settings).await;
    }

    pub(crate) async fn create_terminal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: TerminalCreatePayload,
    ) {
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let launch = resolve_terminal_launch(&project_store, payload).await;
        let terminal_id = TerminalId(Uuid::new_v4().to_string());
        let terminal_stream_path = StreamPath(format!("/terminal/{}", terminal_id));
        let terminal_output_stream = host_output_stream.with_path(terminal_stream_path.clone());
        let terminal = create_terminal(launch, terminal_output_stream)
            .await
            .unwrap_or_else(|err| panic!("failed to create terminal: {err}"));

        {
            let mut state = self.state.lock().await;
            let previous = state.terminal_streams.insert(
                (connection_host_stream.clone(), terminal_id),
                terminal.clone(),
            );
            assert!(
                previous.is_none(),
                "duplicate terminal registration for {}",
                terminal_stream_path
            );
        }

        let host_payload = serde_json::to_value(terminal.new_terminal_payload())
            .expect("failed to serialize new terminal payload");
        if host_output_stream
            .send_value(FrameKind::NewTerminal, host_payload)
            .await
            .is_err()
        {
            return;
        }
        let _ = terminal.emit_start().await;
    }

    pub(crate) async fn send_terminal_input(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalSendPayload,
    ) {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await;
        terminal.send(payload).await;
    }

    pub(crate) async fn resize_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalResizePayload,
    ) {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await;
        terminal.resize(payload.cols, payload.rows).await;
    }

    pub(crate) async fn close_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await;
        terminal.close().await;
    }

    pub(crate) async fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.state.lock().await.registry.agent_handle(agent_id)
    }

    pub(crate) async fn refresh_project(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        _payload: ProjectRefreshPayload,
    ) {
        let (project_store, subscription_state, new_subscription) = {
            let state = self.state.lock().await;
            let project_store = Arc::clone(&state.project_store);
            let key = (
                connection_host_stream.clone(),
                project_output_stream.path().clone(),
            );
            let subscription_state = state
                .project_streams
                .get(&key)
                .map(|subscription| Arc::clone(&subscription.state))
                .unwrap_or_else(|| Arc::new(Mutex::new(ProjectSnapshotState::default())));
            let new_subscription = if state.project_streams.contains_key(&key) {
                None
            } else {
                let task = spawn_project_subscription(
                    Arc::clone(&project_store),
                    project_id.clone(),
                    project_output_stream.clone(),
                    Arc::clone(&subscription_state),
                );
                Some((
                    key,
                    ProjectStreamSubscription {
                        task,
                        state: Arc::clone(&subscription_state),
                    },
                ))
            };
            (project_store, subscription_state, new_subscription)
        };

        let project = project_store
            .lock()
            .await
            .get(&project_id)
            .unwrap_or_else(|| panic!("cannot refresh missing project {}", project_id));
        let file_list = build_file_list(&project)
            .unwrap_or_else(|err| panic!("failed to build project file list: {err}"));
        let git_status = build_git_status(&project)
            .unwrap_or_else(|err| panic!("failed to build project git status: {err}"));
        sync_snapshot_state(&subscription_state, &file_list, &git_status).await;
        if emit_project_file_list(&project_output_stream, &file_list)
            .await
            .is_err()
        {
            return;
        }
        if emit_project_git_status(&project_output_stream, &git_status)
            .await
            .is_err()
        {
            return;
        }

        if let Some((key, subscription)) = new_subscription {
            let mut state = self.state.lock().await;
            state.project_streams.insert(key, subscription);
        }
    }

    pub(crate) async fn read_project_file(
        &self,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadFilePayload,
    ) {
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .get(&project_id)
            .unwrap_or_else(|| panic!("cannot read file from missing project {}", project_id));
        let contents = read_file(&project, payload)
            .unwrap_or_else(|err| panic!("failed to read project file: {err}"));
        let payload = serde_json::to_value(&contents)
            .expect("failed to serialize project file contents payload");
        let _ = project_output_stream
            .send_value(FrameKind::ProjectFileContents, payload)
            .await;
    }

    pub(crate) async fn read_project_diff(
        &self,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadDiffPayload,
    ) {
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .get(&project_id)
            .unwrap_or_else(|| panic!("cannot read diff from missing project {}", project_id));
        let diff = read_diff(&project, payload)
            .unwrap_or_else(|err| panic!("failed to read project diff: {err}"));
        let payload =
            serde_json::to_value(&diff).expect("failed to serialize project git diff payload");
        let _ = project_output_stream
            .send_value(FrameKind::ProjectGitDiff, payload)
            .await;
    }

    pub(crate) async fn stage_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageFilePayload,
    ) {
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .get(&project_id)
            .unwrap_or_else(|| panic!("cannot stage file in missing project {}", project_id));
        stage_file(&project, &path).unwrap_or_else(|err| panic!("failed to stage file: {err}"));
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await;
    }

    pub(crate) async fn stage_project_hunk(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageHunkPayload,
    ) {
        let path = payload.path;
        let hunk_id = payload.hunk_id;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .get(&project_id)
            .unwrap_or_else(|| panic!("cannot stage hunk in missing project {}", project_id));
        stage_hunk(&project, &path, &hunk_id)
            .unwrap_or_else(|err| panic!("failed to stage hunk: {err}"));
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await;
    }
}

pub fn spawn_host() -> HostHandle {
    let session_path = SessionStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default session store path: {err}"));
    let project_path = ProjectStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default project store path: {err}"));
    let settings_path = HostSettingsStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default settings store path: {err}"));
    spawn_host_with_store_paths(session_path, project_path, settings_path)
        .unwrap_or_else(|err| panic!("failed to initialize host stores: {err}"))
}

pub fn spawn_host_with_session_store(path: PathBuf) -> Result<HostHandle, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Session store path has no parent: {}", path.display()))?;
    let project_path = parent.join("projects.json");
    let settings_path = parent.join("settings.json");
    spawn_host_with_store_paths(path, project_path, settings_path)
}

pub fn spawn_host_with_store_paths(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    spawn_host_inner(session_path, project_path, settings_path, false)
}

/// Spawn a host that uses MockBackend for all agent spawns (for tests).
pub fn spawn_host_with_mock_backend(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    spawn_host_inner(session_path, project_path, settings_path, true)
}

fn spawn_host_inner(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
    use_mock_backend: bool,
) -> Result<HostHandle, String> {
    let session_store = SessionStore::load(session_path)?;
    let project_store = ProjectStore::load(project_path)?;
    let settings_store = HostSettingsStore::load(settings_path)?;
    Ok(HostHandle {
        state: Arc::new(Mutex::new(HostState {
            registry: AgentRegistry::new(),
            project_store: Arc::new(Mutex::new(project_store)),
            settings_store: Arc::new(Mutex::new(settings_store)),
            session_store: Arc::new(Mutex::new(session_store)),
            agent_sessions: HashMap::new(),
            use_mock_backend,
            host_streams: HashMap::new(),
            project_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
        })),
    })
}

async fn emit_new_agent_for_subscriber(
    start: &AgentStartPayload,
    agent_handle: &AgentHandle,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let instance_stream = new_instance_stream(&start.agent_id);

    let new_agent = NewAgentPayload {
        agent_id: start.agent_id.clone(),
        name: start.name.clone(),
        backend_kind: start.backend_kind,
        workspace_roots: start.workspace_roots.clone(),
        project_id: start.project_id.clone(),
        parent_agent_id: start.parent_agent_id.clone(),
        created_at_ms: start.created_at_ms,
        instance_stream: instance_stream.clone(),
    };

    let payload = serde_json::to_value(&new_agent)
        .expect("failed to serialize NewAgent payload for host stream fanout");
    subscriber
        .stream
        .send_value(FrameKind::NewAgent, payload)
        .await?;

    let agent_stream = subscriber.stream.with_path(instance_stream);
    let attached = agent_handle.attach(agent_stream).await;
    assert!(
        attached,
        "failed to attach newly spawned agent stream {}; registry is inconsistent",
        start.agent_id
    );

    Ok(())
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
        path: Option<ProjectPath>,
    ) {
        self.refresh_project(
            connection_host_stream,
            project_output_stream.clone(),
            project_id.clone(),
            ProjectRefreshPayload::default(),
        )
        .await;

        if let Some(path) = path {
            let staged_diff = ProjectReadDiffPayload {
                root: path.root.clone(),
                scope: protocol::ProjectDiffScope::Staged,
                path: Some(path.relative_path.clone()),
            };
            self.read_project_diff(&project_output_stream, project_id.clone(), staged_diff)
                .await;

            let unstaged_diff = ProjectReadDiffPayload {
                root: path.root.clone(),
                scope: protocol::ProjectDiffScope::Unstaged,
                path: Some(path.relative_path),
            };
            self.read_project_diff(&project_output_stream, project_id, unstaged_diff)
                .await;
        }
    }

    async fn terminal_handle(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) -> TerminalHandle {
        let state = self.state.lock().await;
        state
            .terminal_streams
            .get(&(connection_host_stream.clone(), terminal_id.clone()))
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "terminal {} is not owned by host stream {}",
                    terminal_id, connection_host_stream
                )
            })
    }
}

async fn emit_project_file_list(
    stream: &Stream,
    payload: &ProjectFileListPayload,
) -> Result<(), StreamClosed> {
    let payload =
        serde_json::to_value(payload).expect("failed to serialize project file list payload");
    stream.send_value(FrameKind::ProjectFileList, payload).await
}

async fn emit_project_git_status(
    stream: &Stream,
    payload: &ProjectGitStatusPayload,
) -> Result<(), StreamClosed> {
    let payload =
        serde_json::to_value(payload).expect("failed to serialize project git status payload");
    stream
        .send_value(FrameKind::ProjectGitStatus, payload)
        .await
}

async fn resolve_terminal_launch(
    project_store: &Arc<Mutex<ProjectStore>>,
    payload: TerminalCreatePayload,
) -> TerminalLaunchInfo {
    match payload.target {
        TerminalLaunchTarget::Project {
            project_id,
            root,
            relative_cwd,
        } => {
            let project = project_store
                .lock()
                .await
                .get(&project_id)
                .unwrap_or_else(|| {
                    panic!("cannot create terminal in missing project {}", project_id)
                });
            let roots = project.roots.iter().cloned().collect::<HashSet<_>>();
            assert!(
                roots.contains(&root.0),
                "cannot create terminal in root {} that is not part of project {}",
                root,
                project_id
            );

            let cwd = resolve_project_terminal_cwd(&root, relative_cwd.as_deref())
                .unwrap_or_else(|err| panic!("invalid terminal launch path: {err}"));
            TerminalLaunchInfo {
                project_id: Some(project_id),
                root: Some(root),
                cwd,
                cols: payload.cols,
                rows: payload.rows,
            }
        }
        TerminalLaunchTarget::Path { cwd } => {
            let trimmed = cwd.trim();
            assert!(!trimmed.is_empty(), "terminal path cwd must not be empty");
            assert!(
                Path::new(trimmed).is_absolute(),
                "terminal path cwd must be absolute: {}",
                trimmed
            );
            TerminalLaunchInfo {
                project_id: None,
                root: None,
                cwd: trimmed.to_owned(),
                cols: payload.cols,
                rows: payload.rows,
            }
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
    assert!(
        relative.is_relative(),
        "terminal relative_cwd must be relative: {}",
        path
    );

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
