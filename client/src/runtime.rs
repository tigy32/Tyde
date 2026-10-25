use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use protocol::types::{AgentClosedPayload, CloseAgentPayload};
use protocol::{
    AgentActivityChangedPayload, AgentActivityStatsPayload, AgentActivitySummaryPayload,
    AgentBootstrapPayload, AgentErrorPayload, AgentRenamedPayload, AgentStartPayload,
    AgentTurnStateNotifyPayload, AgentsViewPreferencesNotifyPayload, BackendCapacityPayload,
    BackendConfigSchemasPayload, BackendConfigSnapshotsPayload, BackendSetupPayload,
    CancelWorkflowPayload, ChatEvent, CodeIntelDiagnosticsPayload, CodeIntelErrorPayload,
    CodeIntelFileModelPayload, CodeIntelHoverResultPayload, CodeIntelNavigateResultPayload,
    CodeIntelOverviewPayload, CodeIntelReferencesCompletePayload,
    CodeIntelReferencesResultsPayload, CodeIntelStatusPayload, CommandErrorPayload,
    ContextCompactionCapabilityPayload, ContextCompactionNotifyPayload, CustomAgentNotifyPayload,
    Envelope, FetchSessionHistoryPayload, FrameError, FrameKind, FrameReader, HostBootstrapPayload,
    HostSettingsPayload, InterruptPayload, LaunchProfileCatalogPayload, ListSessionsPayload,
    McpServerNotifyPayload, MobileAccessStatePayload, MobilePairingOfferPayload, NewAgentPayload,
    NewTerminalPayload, ProjectAccessedPayload, ProjectAddRootPayload, ProjectBootstrapPayload,
    ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRenamePayload, ProjectReorderPayload, ProjectStageFilePayload,
    ProjectStageHunkPayload, QueuedMessagesPayload, SendMessagePayload, SessionHistoryPayload,
    SessionListPayload, SessionSchemasPayload, SessionSettingsPayload,
    SessionSummaryCountUpdatedPayload, SetAgentNamePayload, SetSessionSettingsPayload,
    SkillNotifyPayload, SpawnAgentPayload, SteeringNotifyPayload, StreamPath,
    TaskTokenUsagePayload, TeamContextCompactionNotifyPayload, TeamDraftNotifyPayload,
    TeamMemberBindingNotifyPayload, TeamMemberNotifyPayload,
    TeamMemberShuffleSuggestionNotifyPayload, TeamNotifyPayload, TeamPresetCatalogNotifyPayload,
    TerminalBootstrapPayload, TerminalClosePayload, TerminalCreatePayload, TerminalErrorPayload,
    TerminalExitPayload, TerminalOutputPayload, TerminalResizePayload, TerminalSendPayload,
    TerminalStartPayload, TriggerWorkflowPayload, WorkbenchCreatePayload, WorkbenchRemovePayload,
    WorkflowNotifyPayload, WorkflowRefreshPayload, WorkflowRunNotifyPayload, write_envelope,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{
    ClientConfig, ConnectedParts, HandshakeError, ServerVoiceState, VoiceEvent,
    accept_server_voice_frame, connect_parts, is_voice_frame, parse_agent_stream,
    parse_terminal_stream,
};

const CHANNEL_CAPACITY: usize = 64;

#[derive(Debug)]
pub enum ClientError {
    Frame(FrameError),
    InvalidPayload(serde_json::Error),
    ConnectionClosed,
    StreamAlreadyRegistered(StreamPath),
    UnexpectedFrame { stream: StreamPath, kind: FrameKind },
    AgentIdentityMismatch { stream: StreamPath, kind: FrameKind },
}

impl From<FrameError> for ClientError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

pub struct HostEndpoint {
    pub events: HostEvents,
    pub commands: HostCommands,
}

pub struct AgentEndpoint {
    pub info: NewAgentPayload,
    pub events: AgentEvents,
    pub commands: AgentCommands,
}

pub struct ProjectEndpoint {
    pub project_id: ProjectId,
    pub events: ProjectEvents,
    pub commands: ProjectCommands,
}

pub struct TerminalEndpoint {
    pub info: NewTerminalPayload,
    pub events: TerminalEvents,
    pub commands: TerminalCommands,
}

pub struct HostEvents {
    stream: StreamPath,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<HostEvent>,
}

pub struct AgentEvents {
    stream: StreamPath,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<AgentEvent>,
}

pub struct ProjectEvents {
    stream: StreamPath,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<ProjectEvent>,
}

pub struct TerminalEvents {
    stream: StreamPath,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<TerminalEvent>,
}

#[derive(Clone)]
pub struct HostCommands {
    stream: StreamPath,
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct AgentCommands {
    stream: StreamPath,
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct ProjectCommands {
    stream: StreamPath,
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct TerminalCommands {
    stream: StreamPath,
    shared: Arc<Shared>,
}

pub enum HostEvent {
    ConnectionError(ClientError),
    AgentProjectChanged(Box<AgentStartPayload>),
    HostBootstrap(Box<HostBootstrapPayload>),
    HostSettings(HostSettingsPayload),
    SettingsWriteResult(protocol::SettingsWriteResultPayload),
    AgentActivitySummary(AgentActivitySummaryPayload),
    TaskTokenUsage(Box<TaskTokenUsagePayload>),
    AgentsViewPreferencesNotify(AgentsViewPreferencesNotifyPayload),
    BackendSetup(BackendSetupPayload),
    BackendConfigSchemas(BackendConfigSchemasPayload),
    BackendConfigSnapshots(BackendConfigSnapshotsPayload),
    BackendCapacity(BackendCapacityPayload),
    LaunchProfileCatalogNotify(LaunchProfileCatalogPayload),
    SessionSchemas(SessionSchemasPayload),
    SessionList(SessionListPayload),
    SessionSummaryCountUpdated(SessionSummaryCountUpdatedPayload),
    CommandError(CommandErrorPayload),
    ProjectNotify(ProjectNotifyPayload),
    ProjectOperation(protocol::ProjectOperationPayload),
    CustomAgentNotify(CustomAgentNotifyPayload),
    SteeringNotify(SteeringNotifyPayload),
    SelfImprovementNotify(protocol::SelfImprovementState),
    SkillNotify(SkillNotifyPayload),
    McpServerNotify(McpServerNotifyPayload),
    TychatState(protocol::TychatStatePayload),
    MobileAccessState(MobileAccessStatePayload),
    MobilePairingOffer(MobilePairingOfferPayload),
    SwarmNotify(protocol::SwarmNotifyPayload),
    SwarmDraftNotify(protocol::SwarmDraftNotifyPayload),
    SwarmPostNotify(protocol::SwarmPostNotifyPayload),
    SwarmBoardNotify(protocol::SwarmBoardNotifyPayload),
    SwarmThreadNotify(protocol::SwarmThreadNotifyPayload),
    SwarmErrorNotify(protocol::SwarmErrorNotifyPayload),
    TeamNotify(TeamNotifyPayload),
    TeamsStoreStatusNotify(protocol::TeamsStoreStatusNotifyPayload),
    AgentRestorationStatus(protocol::AgentRestorationStatusPayload),
    TeamMemberNotify(TeamMemberNotifyPayload),
    TeamMemberBindingNotify(TeamMemberBindingNotifyPayload),
    TeamPresetCatalogNotify(TeamPresetCatalogNotifyPayload),
    TeamDraftNotify(TeamDraftNotifyPayload),
    TeamMemberShuffleSuggestionNotify(TeamMemberShuffleSuggestionNotifyPayload),
    TeamContextCompactionNotify(TeamContextCompactionNotifyPayload),
    WorkflowNotify(WorkflowNotifyPayload),
    WorkflowRunNotify(WorkflowRunNotifyPayload),
    AgentMoveResult(protocol::types::AgentMoveResultPayload),
    AgentClosed(AgentClosedPayload),
    NewAgent(AgentEndpoint),
    NewTerminal(TerminalEndpoint),
    Voice(VoiceEvent),
}

pub enum AgentEvent {
    Bootstrap(AgentBootstrapPayload),
    Start(Box<AgentStartPayload>),
    Renamed(AgentRenamedPayload),
    Error(AgentErrorPayload),
    ActivityStats(AgentActivityStatsPayload),
    ActivityChanged(AgentActivityChangedPayload),
    ContextCompactionNotify(ContextCompactionNotifyPayload),
    ContextCompactionCapability(ContextCompactionCapabilityPayload),
    Chat(Box<ChatEvent>),
    SessionHistory(SessionHistoryPayload),
    SessionSettings(SessionSettingsPayload),
    QueuedMessages(QueuedMessagesPayload),
}

pub enum ProjectEvent {
    Bootstrap(ProjectBootstrapPayload),
    FileList(ProjectFileListPayload),
    GitStatus(ProjectGitStatusPayload),
    CodeIntelOverview(CodeIntelOverviewPayload),
    CodeIntelStatus(CodeIntelStatusPayload),
    CodeIntelFileModel(CodeIntelFileModelPayload),
    CodeIntelDiagnostics(CodeIntelDiagnosticsPayload),
    CodeIntelHoverResult(CodeIntelHoverResultPayload),
    CodeIntelNavigateResult(CodeIntelNavigateResultPayload),
    CodeIntelReferencesResults(CodeIntelReferencesResultsPayload),
    CodeIntelReferencesComplete(CodeIntelReferencesCompletePayload),
    CodeIntelError(CodeIntelErrorPayload),
    FileContents(ProjectFileContentsPayload),
    GitDiff(ProjectGitDiffPayload),
    Notify(ProjectEventPayload),
}

pub enum TerminalEvent {
    Bootstrap(TerminalBootstrapPayload),
    Start(TerminalStartPayload),
    Output(TerminalOutputPayload),
    Exit(TerminalExitPayload),
    Error(TerminalErrorPayload),
}

pub async fn connect_host_endpoint<S>(
    config: &ClientConfig,
    stream: S,
) -> Result<HostEndpoint, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let parts = connect_parts(config, stream).await?;
    Ok(spawn_runtime(parts))
}

pub async fn connect_uds_host_endpoint(
    path: impl AsRef<std::path::Path>,
    config: &ClientConfig,
) -> Result<HostEndpoint, HandshakeError> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|err| HandshakeError::Frame(FrameError::Io(err)))?;
        connect_host_endpoint(config, stream).await
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = config;
        Err(HandshakeError::Frame(FrameError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unix domain sockets are not supported on this platform",
        ))))
    }
}

impl HostEvents {
    pub async fn recv(&mut self) -> Option<HostEvent> {
        self.rx.recv().await
    }
}

impl AgentEvents {
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.rx.recv().await
    }
}

impl ProjectEvents {
    pub async fn recv(&mut self) -> Option<ProjectEvent> {
        self.rx.recv().await
    }
}

impl TerminalEvents {
    pub async fn recv(&mut self) -> Option<TerminalEvent> {
        self.rx.recv().await
    }
}

impl HostCommands {
    pub async fn swarm_command(
        &self,
        payload: protocol::SwarmCommandPayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::SwarmCommand, &payload).await
    }
    pub async fn spawn_agent(&self, payload: SpawnAgentPayload) -> Result<(), ClientError> {
        self.send(FrameKind::SpawnAgent, &payload).await
    }

    pub async fn list_sessions(&self) -> Result<(), ClientError> {
        self.send(FrameKind::ListSessions, &ListSessionsPayload::default())
            .await
    }

    pub async fn project_create(&self, payload: ProjectCreatePayload) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectCreate, &payload).await
    }

    pub async fn move_agent(
        &self,
        payload: protocol::types::AgentMovePayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::AgentMove, &payload).await
    }

    pub async fn project_rename(&self, payload: ProjectRenamePayload) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectRename, &payload).await
    }

    pub async fn project_reorder(&self, payload: ProjectReorderPayload) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectReorder, &payload).await
    }

    pub async fn project_add_root(
        &self,
        payload: ProjectAddRootPayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectAddRoot, &payload).await
    }

    pub async fn project_delete_root(
        &self,
        payload: ProjectDeleteRootPayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectDeleteRoot, &payload).await
    }

    pub async fn project_delete(&self, payload: ProjectDeletePayload) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectDelete, &payload).await
    }

    pub async fn workbench_create(
        &self,
        payload: WorkbenchCreatePayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::WorkbenchCreate, &payload).await
    }

    pub async fn workbench_remove(
        &self,
        payload: WorkbenchRemovePayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::WorkbenchRemove, &payload).await
    }

    pub async fn terminal_create(&self, payload: TerminalCreatePayload) -> Result<(), ClientError> {
        self.send(FrameKind::TerminalCreate, &payload).await
    }

    pub async fn team_member_shuffle(
        &self,
        payload: protocol::TeamMemberShufflePayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::TeamMemberShuffle, &payload).await
    }

    pub async fn backend_capacity_refresh(
        &self,
        payload: protocol::BackendCapacityRefreshPayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::BackendCapacityRefresh, &payload).await
    }

    pub async fn workflow_refresh(&self) -> Result<(), ClientError> {
        self.send(
            FrameKind::WorkflowRefresh,
            &WorkflowRefreshPayload::default(),
        )
        .await
    }

    pub async fn trigger_workflow(
        &self,
        payload: TriggerWorkflowPayload,
    ) -> Result<(), ClientError> {
        self.send(FrameKind::TriggerWorkflow, &payload).await
    }

    pub async fn cancel_workflow(&self, payload: CancelWorkflowPayload) -> Result<(), ClientError> {
        self.send(FrameKind::CancelWorkflow, &payload).await
    }

    pub async fn open_project(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectEndpoint, ClientError> {
        let stream = project_stream(&project_id);
        if let Some(rx) = self.shared.take_pending_project_events(&project_id) {
            let endpoint = ProjectEndpoint {
                project_id,
                events: ProjectEvents {
                    stream: stream.clone(),
                    shared: self.shared.clone(),
                    rx,
                },
                commands: ProjectCommands {
                    stream,
                    shared: self.shared.clone(),
                },
            };
            endpoint.commands.accessed().await?;
            return Ok(endpoint);
        }

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        self.shared
            .register_route(stream.clone(), Route::Project(tx))?;
        self.shared
            .register_writable_stream(stream.clone(), 0)
            .await?;

        let endpoint = ProjectEndpoint {
            project_id,
            events: ProjectEvents {
                stream: stream.clone(),
                shared: self.shared.clone(),
                rx,
            },
            commands: ProjectCommands {
                stream,
                shared: self.shared.clone(),
            },
        };
        endpoint.commands.accessed().await?;
        Ok(endpoint)
    }

    async fn send<T: Serialize>(&self, kind: FrameKind, payload: &T) -> Result<(), ClientError> {
        self.shared.send(self.stream.clone(), kind, payload).await
    }
}

impl AgentCommands {
    pub async fn send_message(&self, payload: SendMessagePayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::SendMessage, &payload)
            .await
    }

    pub async fn fetch_session_history(
        &self,
        payload: FetchSessionHistoryPayload,
    ) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::FetchSessionHistory,
                &payload,
            )
            .await
    }

    pub async fn set_session_settings(
        &self,
        payload: SetSessionSettingsPayload,
    ) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::SetSessionSettings, &payload)
            .await
    }

    pub async fn set_name(&self, name: String) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::SetAgentName,
                &SetAgentNamePayload { name },
            )
            .await
    }

    pub async fn interrupt(&self) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::Interrupt,
                &InterruptPayload::default(),
            )
            .await
    }

    pub async fn close(&self) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::CloseAgent,
                &CloseAgentPayload::default(),
            )
            .await
    }
}

impl ProjectCommands {
    pub async fn accessed(&self) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::ProjectAccessed,
                &ProjectAccessedPayload::default(),
            )
            .await
    }

    pub async fn read_file(&self, payload: ProjectReadFilePayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::ProjectReadFile, &payload)
            .await
    }

    pub async fn read_diff(&self, payload: ProjectReadDiffPayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::ProjectReadDiff, &payload)
            .await
    }

    pub async fn stage_file(&self, payload: ProjectStageFilePayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::ProjectStageFile, &payload)
            .await
    }

    pub async fn stage_hunk(&self, payload: ProjectStageHunkPayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::ProjectStageHunk, &payload)
            .await
    }
}

impl TerminalCommands {
    pub async fn send(&self, payload: TerminalSendPayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::TerminalSend, &payload)
            .await
    }

    pub async fn resize(&self, payload: TerminalResizePayload) -> Result<(), ClientError> {
        self.shared
            .send(self.stream.clone(), FrameKind::TerminalResize, &payload)
            .await
    }

    pub async fn close(&self) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::TerminalClose,
                &TerminalClosePayload::default(),
            )
            .await
    }
}

impl Drop for HostEvents {
    fn drop(&mut self) {
        self.shared.unregister_route(&self.stream);
    }
}

impl Drop for AgentEvents {
    fn drop(&mut self) {
        self.shared.unregister_route(&self.stream);
    }
}

impl Drop for ProjectEvents {
    fn drop(&mut self) {
        self.shared.unregister_route(&self.stream);
    }
}

impl Drop for TerminalEvents {
    fn drop(&mut self) {
        self.shared.unregister_route(&self.stream);
    }
}

struct Shared {
    routes: Mutex<HashMap<StreamPath, Route>>,
    pending_project_events: Mutex<HashMap<ProjectId, mpsc::Receiver<ProjectEvent>>>,
    write_tx: mpsc::Sender<WriteRequest>,
    _read_task: JoinHandle<()>,
    _write_task: JoinHandle<()>,
}

struct PendingAgentEndpoint {
    payload: NewAgentPayload,
    events: mpsc::Receiver<AgentEvent>,
    host_tx: mpsc::Sender<HostEvent>,
}

enum Route {
    Host,
    Agent(mpsc::Sender<AgentEvent>),
    Project(mpsc::Sender<ProjectEvent>),
    Terminal(mpsc::Sender<TerminalEvent>),
}

enum WriteRequest {
    RegisterStream {
        stream: StreamPath,
        initial_seq: u64,
    },
    Send {
        stream: StreamPath,
        kind: FrameKind,
        payload: serde_json::Value,
        result_tx: oneshot::Sender<Result<(), ClientError>>,
    },
}

fn spawn_runtime(parts: ConnectedParts) -> HostEndpoint {
    let (host_tx, host_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (write_tx, write_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (writer_shutdown_tx, writer_shutdown_rx) = oneshot::channel();

    let shared = Arc::new_cyclic(|weak| {
        let write_task = tokio::spawn(write_pump(
            parts.writer,
            parts.outgoing_seq,
            write_rx,
            writer_shutdown_rx,
        ));
        let read_task = tokio::spawn(read_pump(
            parts.reader,
            parts.incoming_seq,
            parts.host_stream.clone(),
            host_tx.clone(),
            weak.clone(),
            writer_shutdown_tx,
        ));

        let mut routes = HashMap::new();
        routes.insert(parts.host_stream.clone(), Route::Host);

        Shared {
            routes: Mutex::new(routes),
            pending_project_events: Mutex::new(HashMap::new()),
            write_tx,
            _read_task: read_task,
            _write_task: write_task,
        }
    });

    HostEndpoint {
        events: HostEvents {
            stream: parts.host_stream.clone(),
            shared: shared.clone(),
            rx: host_rx,
        },
        commands: HostCommands {
            stream: parts.host_stream,
            shared,
        },
    }
}

impl Shared {
    fn register_route(&self, stream: StreamPath, route: Route) -> Result<(), ClientError> {
        let mut routes = self.routes.lock().expect("route registry poisoned");
        if routes.contains_key(&stream) {
            return Err(ClientError::StreamAlreadyRegistered(stream));
        }
        routes.insert(stream, route);
        Ok(())
    }

    fn unregister_route(&self, stream: &StreamPath) {
        let mut routes = self.routes.lock().expect("route registry poisoned");
        routes.remove(stream);
    }

    async fn pre_register_project_endpoint(
        &self,
        project_id: ProjectId,
    ) -> Result<(), ClientError> {
        let stream = project_stream(&project_id);
        {
            let routes = self.routes.lock().expect("route registry poisoned");
            if routes.contains_key(&stream) {
                return Ok(());
            }
        }

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        {
            let mut routes = self.routes.lock().expect("route registry poisoned");
            if routes.contains_key(&stream) {
                return Ok(());
            }
            routes.insert(stream.clone(), Route::Project(tx));
        }
        {
            let mut pending = self
                .pending_project_events
                .lock()
                .expect("pending project registry poisoned");
            pending.insert(project_id.clone(), rx);
        }

        if let Err(error) = self.register_writable_stream(stream.clone(), 0).await {
            self.unregister_route(&stream);
            self.pending_project_events
                .lock()
                .expect("pending project registry poisoned")
                .remove(&project_id);
            return Err(error);
        }

        Ok(())
    }

    fn take_pending_project_events(
        &self,
        project_id: &ProjectId,
    ) -> Option<mpsc::Receiver<ProjectEvent>> {
        self.pending_project_events
            .lock()
            .expect("pending project registry poisoned")
            .remove(project_id)
    }

    fn remove_pending_project_endpoint(&self, project_id: &ProjectId) {
        let stream = project_stream(project_id);
        let removed = self
            .pending_project_events
            .lock()
            .expect("pending project registry poisoned")
            .remove(project_id)
            .is_some();
        if removed {
            self.unregister_route(&stream);
        }
    }

    async fn register_writable_stream(
        &self,
        stream: StreamPath,
        initial_seq: u64,
    ) -> Result<(), ClientError> {
        self.write_tx
            .send(WriteRequest::RegisterStream {
                stream,
                initial_seq,
            })
            .await
            .map_err(|_| ClientError::ConnectionClosed)
    }

    async fn send<T: Serialize>(
        &self,
        stream: StreamPath,
        kind: FrameKind,
        payload: &T,
    ) -> Result<(), ClientError> {
        let payload = serde_json::to_value(payload).map_err(ClientError::InvalidPayload)?;
        let (result_tx, result_rx) = oneshot::channel();
        self.write_tx
            .send(WriteRequest::Send {
                stream,
                kind,
                payload,
                result_tx,
            })
            .await
            .map_err(|_| ClientError::ConnectionClosed)?;
        result_rx.await.map_err(|_| ClientError::ConnectionClosed)?
    }

    fn route_for(&self, stream: &StreamPath) -> Option<RouteRef> {
        let routes = self.routes.lock().expect("route registry poisoned");
        let route = routes.get(stream)?;
        Some(match route {
            Route::Host => return None,
            Route::Agent(tx) => RouteRef::Agent(tx.clone()),
            Route::Project(tx) => RouteRef::Project(tx.clone()),
            Route::Terminal(tx) => RouteRef::Terminal(tx.clone()),
        })
    }
}

enum RouteRef {
    Agent(mpsc::Sender<AgentEvent>),
    Project(mpsc::Sender<ProjectEvent>),
    Terminal(mpsc::Sender<TerminalEvent>),
}

async fn read_pump(
    mut reader: FrameReader<Box<dyn tokio::io::AsyncBufRead + Unpin + Send>>,
    mut incoming_seq: protocol::SeqValidator,
    host_stream: StreamPath,
    host_tx: mpsc::Sender<HostEvent>,
    shared: std::sync::Weak<Shared>,
    writer_shutdown: oneshot::Sender<()>,
) {
    let result = async {
        let mut voice_state = ServerVoiceState::default();
        let mut pending_agent_endpoints = HashMap::new();
        loop {
            let frame = reader
                .read_frame()
                .await?
                .ok_or(ClientError::ConnectionClosed)?;
            let envelope = &frame.envelope;
            incoming_seq
                .validate(&envelope.stream, envelope.seq, envelope.kind)
                .map_err(FrameError::from)?;
            let Some(shared) = shared.upgrade() else {
                return Ok(());
            };
            if is_voice_frame(&frame) {
                let event = accept_server_voice_frame(&mut voice_state, &host_stream, &frame)?
                    .ok_or_else(|| ClientError::UnexpectedFrame {
                        stream: envelope.stream.clone(),
                        kind: envelope.kind,
                    })?;
                host_tx
                    .send(HostEvent::Voice(event))
                    .await
                    .map_err(|_| ClientError::ConnectionClosed)?;
                continue;
            }
            if !frame.binary.is_empty() {
                return Err(ClientError::UnexpectedFrame {
                    stream: envelope.stream.clone(),
                    kind: envelope.kind,
                });
            }
            let envelope = frame.envelope;
            if envelope.stream == host_stream {
                if !handle_host_envelope(
                    envelope,
                    host_tx.clone(),
                    shared,
                    &mut pending_agent_endpoints,
                )
                .await?
                {
                    return Err(ClientError::ConnectionClosed);
                }
            } else if envelope.stream.0.starts_with("/agent/") {
                handle_agent_envelope(envelope, &shared, &mut pending_agent_endpoints).await?;
            } else if envelope.stream.0.starts_with("/project/") {
                handle_project_envelope(envelope, &shared).await?;
            } else if envelope.stream.0.starts_with("/terminal/") {
                handle_terminal_envelope(envelope, &shared).await?;
            } else {
                return Err(ClientError::UnexpectedFrame {
                    stream: envelope.stream,
                    kind: envelope.kind,
                });
            }
        }
    }
    .await;
    let _ = writer_shutdown.send(());
    if let Err(error) = result {
        let _ = host_tx.send(HostEvent::ConnectionError(error)).await;
    }
}

async fn write_pump(
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    mut outgoing_seq: HashMap<StreamPath, u64>,
    mut rx: mpsc::Receiver<WriteRequest>,
    shutdown: oneshot::Receiver<()>,
) {
    let write_requests = async {
        while let Some(request) = rx.recv().await {
            match request {
                WriteRequest::RegisterStream {
                    stream,
                    initial_seq,
                } => {
                    outgoing_seq.entry(stream).or_insert(initial_seq);
                }
                WriteRequest::Send {
                    stream,
                    kind,
                    payload,
                    result_tx,
                } => {
                    let result = match outgoing_seq.get_mut(&stream) {
                        Some(seq) => {
                            let envelope = Envelope {
                                stream: stream.clone(),
                                kind,
                                seq: *seq,
                                payload,
                            };
                            *seq += 1;
                            write_envelope(&mut writer, &envelope)
                                .await
                                .map_err(ClientError::Frame)
                        }
                        None => Err(ClientError::ConnectionClosed),
                    };
                    let _ = result_tx.send(result);
                }
            }
        }
    };
    tokio::select! {
        biased;
        _ = shutdown => {},
        _ = write_requests => {},
    }
}

async fn handle_host_envelope(
    envelope: Envelope,
    host_tx: mpsc::Sender<HostEvent>,
    shared: Arc<Shared>,
    pending_agent_endpoints: &mut HashMap<StreamPath, PendingAgentEndpoint>,
) -> Result<bool, ClientError> {
    let handled = match envelope.kind {
        FrameKind::HostBootstrap => {
            let payload: HostBootstrapPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let agents = payload.agents.clone();
            let projects = payload.projects.clone();
            for project in &projects {
                if shared
                    .pre_register_project_endpoint(project.id.clone())
                    .await
                    .is_err()
                {
                    return Ok(false);
                }
            }
            let _ = host_tx
                .send(HostEvent::HostBootstrap(Box::new(payload)))
                .await;
            for agent in agents {
                if !emit_new_agent_endpoint(agent, &host_tx, &shared, pending_agent_endpoints)
                    .await?
                {
                    return Ok(false);
                }
            }
            true
        }
        FrameKind::HostSettings => {
            let payload: HostSettingsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::HostSettings(payload)).await;
            true
        }
        FrameKind::SettingsWriteResult => {
            let payload: protocol::SettingsWriteResultPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SettingsWriteResult(payload)).await;
            true
        }
        FrameKind::AgentActivitySummary => {
            let payload: AgentActivitySummaryPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::AgentActivitySummary(payload)).await;
            true
        }
        FrameKind::AgentBackgroundWorkNotify => envelope
            .parse_payload::<protocol::AgentBackgroundWorkNotifyPayload>()
            .is_ok(),
        FrameKind::AgentTurnStateNotify => {
            // Liveness for agents this client has not attached; the runtime
            // attaches every agent it is told about, so it has no use for it.
            envelope
                .parse_payload::<AgentTurnStateNotifyPayload>()
                .is_ok()
        }
        FrameKind::TaskTokenUsage => {
            let payload: TaskTokenUsagePayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TaskTokenUsage(Box::new(payload)))
                .await;
            true
        }
        FrameKind::AgentsViewPreferencesNotify => {
            let payload: AgentsViewPreferencesNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::AgentsViewPreferencesNotify(payload))
                .await;
            true
        }
        FrameKind::BackendSetup => {
            let payload: BackendSetupPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::BackendSetup(payload)).await;
            true
        }
        FrameKind::BackendConfigSchemas => {
            let payload: BackendConfigSchemasPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::BackendConfigSchemas(payload)).await;
            true
        }
        FrameKind::BackendConfigSnapshots => {
            let payload: BackendConfigSnapshotsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::BackendConfigSnapshots(payload))
                .await;
            true
        }
        FrameKind::BackendCapacity => {
            let payload: BackendCapacityPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::BackendCapacity(payload)).await;
            true
        }
        FrameKind::TychatState => {
            let payload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::TychatState(payload)).await;
            true
        }
        FrameKind::MobileAccessState => {
            let payload: MobileAccessStatePayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::MobileAccessState(payload)).await;
            true
        }
        FrameKind::MobilePairingOffer => {
            let payload: MobilePairingOfferPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::MobilePairingOffer(payload)).await;
            true
        }
        FrameKind::SessionList => {
            let payload: SessionListPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SessionList(payload)).await;
            true
        }
        FrameKind::SessionSummaryCountUpdated => {
            let payload: SessionSummaryCountUpdatedPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::SessionSummaryCountUpdated(payload))
                .await;
            true
        }
        FrameKind::ProjectOperation => {
            let payload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::ProjectOperation(payload)).await;
            true
        }
        FrameKind::ProjectNotify => {
            let payload: ProjectNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            match &payload {
                ProjectNotifyPayload::Upsert { project } => {
                    if shared
                        .pre_register_project_endpoint(project.id.clone())
                        .await
                        .is_err()
                    {
                        return Ok(false);
                    }
                }
                ProjectNotifyPayload::Delete { project } => {
                    shared.remove_pending_project_endpoint(&project.id);
                }
            }
            let _ = host_tx.send(HostEvent::ProjectNotify(payload)).await;
            true
        }
        FrameKind::CustomAgentNotify => {
            let payload: CustomAgentNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::CustomAgentNotify(payload)).await;
            true
        }
        FrameKind::SelfImprovementNotify => {
            let payload: protocol::SelfImprovementState = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::SelfImprovementNotify(payload))
                .await;
            true
        }
        FrameKind::SteeringNotify => {
            let payload: SteeringNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SteeringNotify(payload)).await;
            true
        }
        FrameKind::SkillNotify => {
            let payload: SkillNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SkillNotify(payload)).await;
            true
        }
        FrameKind::McpServerNotify => {
            let payload: McpServerNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::McpServerNotify(payload)).await;
            true
        }
        FrameKind::WorkflowNotify => {
            let payload: WorkflowNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::WorkflowNotify(payload)).await;
            true
        }
        FrameKind::WorkflowRunNotify => {
            let payload: WorkflowRunNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::WorkflowRunNotify(payload)).await;
            true
        }
        FrameKind::SwarmNotify => {
            let payload: protocol::SwarmNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmNotify(payload)).await;
            true
        }
        FrameKind::SwarmDraftNotify => {
            let payload: protocol::SwarmDraftNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmDraftNotify(payload)).await;
            true
        }
        FrameKind::SwarmPostNotify => {
            let payload: protocol::SwarmPostNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmPostNotify(payload)).await;
            true
        }
        FrameKind::SwarmBoardNotify => {
            let payload: protocol::SwarmBoardNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmBoardNotify(payload)).await;
            true
        }
        FrameKind::SwarmThreadNotify => {
            let payload: protocol::SwarmThreadNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmThreadNotify(payload)).await;
            true
        }
        FrameKind::SwarmErrorNotify => {
            let payload: protocol::SwarmErrorNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SwarmErrorNotify(payload)).await;
            true
        }
        FrameKind::TeamNotify => {
            let payload: TeamNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::TeamNotify(payload)).await;
            true
        }
        FrameKind::TeamsStoreStatusNotify => {
            let payload: protocol::TeamsStoreStatusNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TeamsStoreStatusNotify(payload))
                .await;
            true
        }
        FrameKind::AgentRestorationStatus => {
            let payload: protocol::AgentRestorationStatusPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::AgentRestorationStatus(payload))
                .await;
            true
        }
        FrameKind::TeamMemberNotify => {
            let payload: TeamMemberNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::TeamMemberNotify(payload)).await;
            true
        }
        FrameKind::TeamMemberBindingNotify => {
            let payload: TeamMemberBindingNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TeamMemberBindingNotify(payload))
                .await;
            true
        }
        FrameKind::TeamPresetCatalogNotify => {
            let payload: TeamPresetCatalogNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TeamPresetCatalogNotify(payload))
                .await;
            true
        }
        FrameKind::TeamDraftNotify => {
            let payload: TeamDraftNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::TeamDraftNotify(payload)).await;
            true
        }
        FrameKind::TeamMemberShuffleSuggestionNotify => {
            let payload: TeamMemberShuffleSuggestionNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TeamMemberShuffleSuggestionNotify(payload))
                .await;
            true
        }
        FrameKind::TeamContextCompactionNotify => {
            let payload: TeamContextCompactionNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::TeamContextCompactionNotify(payload))
                .await;
            true
        }
        FrameKind::SessionSchemas => {
            let payload: SessionSchemasPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::SessionSchemas(payload)).await;
            true
        }
        FrameKind::LaunchProfileCatalogNotify => {
            let payload: LaunchProfileCatalogPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx
                .send(HostEvent::LaunchProfileCatalogNotify(payload))
                .await;
            true
        }
        FrameKind::CommandError => {
            let payload: CommandErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::CommandError(payload)).await;
            true
        }
        FrameKind::AgentProjectChanged => {
            let payload: AgentStartPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            for pending in pending_agent_endpoints.values_mut() {
                if pending.payload.agent_id == payload.agent_id {
                    pending.payload.project_id = payload.project_id.clone();
                    pending.payload.workspace_roots = payload.workspace_roots.clone();
                }
            }
            let _ = host_tx
                .send(HostEvent::AgentProjectChanged(Box::new(payload)))
                .await;
            true
        }
        FrameKind::AgentMoveResult => {
            let payload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::AgentMoveResult(payload)).await;
            true
        }
        FrameKind::AgentClosed => {
            let payload: AgentClosedPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let _ = host_tx.send(HostEvent::AgentClosed(payload)).await;
            true
        }
        FrameKind::NewAgent => {
            let payload: NewAgentPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            emit_new_agent_endpoint(payload, &host_tx, &shared, pending_agent_endpoints).await?
        }
        FrameKind::NewTerminal => {
            let payload: NewTerminalPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream = payload.stream.clone();
            let stream_parts = parse_terminal_stream(&stream);
            assert_eq!(
                payload.terminal_id, stream_parts.terminal_id,
                "new_terminal payload terminal_id {} does not match stream {}",
                payload.terminal_id, stream
            );

            let (terminal_tx, terminal_rx) = mpsc::channel(CHANNEL_CAPACITY);
            if shared
                .register_route(stream.clone(), Route::Terminal(terminal_tx))
                .is_err()
            {
                return Ok(false);
            }
            if shared
                .register_writable_stream(stream.clone(), 0)
                .await
                .is_err()
            {
                shared.unregister_route(&stream);
                return Ok(false);
            }

            let endpoint = TerminalEndpoint {
                info: payload,
                events: TerminalEvents {
                    stream: stream.clone(),
                    shared: shared.clone(),
                    rx: terminal_rx,
                },
                commands: TerminalCommands {
                    stream: stream.clone(),
                    shared: shared.clone(),
                },
            };
            if host_tx
                .send(HostEvent::NewTerminal(endpoint))
                .await
                .is_err()
            {
                shared.unregister_route(&stream);
            }
            true
        }
        kind => {
            return Err(ClientError::UnexpectedFrame {
                stream: envelope.stream,
                kind,
            });
        }
    };
    Ok(handled)
}

async fn emit_new_agent_endpoint(
    payload: NewAgentPayload,
    host_tx: &mpsc::Sender<HostEvent>,
    shared: &Arc<Shared>,
    pending_agent_endpoints: &mut HashMap<StreamPath, PendingAgentEndpoint>,
) -> Result<bool, ClientError> {
    let stream = payload.instance_stream.clone();
    let stream_parts = parse_agent_stream(&stream);
    assert_eq!(
        payload.agent_id, stream_parts.agent_id,
        "new_agent payload agent_id {} does not match instance_stream {}",
        payload.agent_id, stream
    );

    let (agent_tx, agent_rx) = mpsc::channel(CHANNEL_CAPACITY);
    if shared
        .register_route(stream.clone(), Route::Agent(agent_tx))
        .is_err()
    {
        return Ok(false);
    }
    if shared
        .register_writable_stream(stream.clone(), 0)
        .await
        .is_err()
    {
        shared.unregister_route(&stream);
        return Ok(false);
    }

    if payload.session_id.is_none() {
        // Keep wire-level NewAgent early while exposing the authoritative
        // session id on the higher-level endpoint once its bootstrap arrives.
        pending_agent_endpoints.insert(
            stream,
            PendingAgentEndpoint {
                payload,
                events: agent_rx,
                host_tx: host_tx.clone(),
            },
        );
        return Ok(true);
    }

    let endpoint = AgentEndpoint {
        info: payload,
        events: AgentEvents {
            stream: stream.clone(),
            shared: shared.clone(),
            rx: agent_rx,
        },
        commands: AgentCommands {
            stream: stream.clone(),
            shared: shared.clone(),
        },
    };
    if host_tx.send(HostEvent::NewAgent(endpoint)).await.is_err() {
        shared.unregister_route(&stream);
    }
    Ok(true)
}

async fn resolve_pending_agent_endpoint(
    shared: &Arc<Shared>,
    pending_agent_endpoints: &mut HashMap<StreamPath, PendingAgentEndpoint>,
    stream: &StreamPath,
    session_id: Option<protocol::SessionId>,
) {
    let pending = pending_agent_endpoints.remove(stream);
    let Some(mut pending) = pending else {
        return;
    };
    pending.payload.session_id = session_id;
    let endpoint = AgentEndpoint {
        info: pending.payload,
        events: AgentEvents {
            stream: stream.clone(),
            shared: shared.clone(),
            rx: pending.events,
        },
        commands: AgentCommands {
            stream: stream.clone(),
            shared: shared.clone(),
        },
    };
    if pending
        .host_tx
        .send(HostEvent::NewAgent(endpoint))
        .await
        .is_err()
    {
        shared.unregister_route(stream);
    }
}

fn agent_event_session_id(event: &AgentEvent) -> Option<protocol::SessionId> {
    match event {
        AgentEvent::Start(payload) => payload.session_id.clone(),
        AgentEvent::Bootstrap(payload) => bootstrap_agent_session_id(payload),
        AgentEvent::ContextCompactionNotify(payload) => Some(payload.logical_session_id.clone()),
        AgentEvent::ContextCompactionCapability(payload) => {
            Some(payload.logical_session_id.clone())
        }
        AgentEvent::Renamed(_)
        | AgentEvent::Error(_)
        | AgentEvent::ActivityStats(_)
        | AgentEvent::ActivityChanged(_)
        | AgentEvent::Chat(_)
        | AgentEvent::SessionHistory(_)
        | AgentEvent::SessionSettings(_)
        | AgentEvent::QueuedMessages(_) => None,
    }
}

fn bootstrap_agent_session_id(payload: &AgentBootstrapPayload) -> Option<protocol::SessionId> {
    let mut logical_session_id: Option<protocol::SessionId> = None;
    for event in &payload.events {
        let candidate = match event {
            protocol::AgentBootstrapEvent::AgentStart(payload) => payload.session_id.as_ref(),
            protocol::AgentBootstrapEvent::ContextCompaction(payload) => {
                Some(&payload.logical_session_id)
            }
            protocol::AgentBootstrapEvent::ContextCompactionCapability(payload) => {
                Some(&payload.logical_session_id)
            }
            protocol::AgentBootstrapEvent::AgentError(_)
            | protocol::AgentBootstrapEvent::SessionSettings(_)
            | protocol::AgentBootstrapEvent::QueuedMessages(_)
            | protocol::AgentBootstrapEvent::AgentActivityStats(_)
            | protocol::AgentBootstrapEvent::ChatEvent(_)
            | protocol::AgentBootstrapEvent::HasPriorHistory { .. } => None,
        };
        let Some(candidate) = candidate else {
            continue;
        };
        if let Some(current) = logical_session_id.as_ref()
            && current != candidate
        {
            tracing::error!(
                "agent bootstrap carried conflicting logical sessions {} and {}",
                current.0,
                candidate.0,
            );
            return None;
        }
        logical_session_id = Some(candidate.clone());
    }
    logical_session_id
}

async fn handle_agent_envelope(
    envelope: Envelope,
    shared: &Arc<Shared>,
    pending_agent_endpoints: &mut HashMap<StreamPath, PendingAgentEndpoint>,
) -> Result<(), ClientError> {
    let Some(RouteRef::Agent(tx)) = shared.route_for(&envelope.stream) else {
        return Ok(());
    };

    let event = match envelope.kind {
        FrameKind::AgentBootstrap => {
            let payload: AgentBootstrapPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::Bootstrap(payload)
        }
        FrameKind::AgentStart | FrameKind::AgentProjectChanged => {
            let payload: AgentStartPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            if payload.agent_id != stream_parts.agent_id {
                return Err(ClientError::AgentIdentityMismatch {
                    stream: envelope.stream,
                    kind: envelope.kind,
                });
            }
            AgentEvent::Start(Box::new(payload))
        }
        FrameKind::AgentError => {
            let payload: AgentErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "agent_error payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::Error(payload)
        }
        FrameKind::AgentRenamed => {
            let payload: AgentRenamedPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "agent_renamed payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::Renamed(payload)
        }
        FrameKind::AgentActivityStats => {
            let payload: AgentActivityStatsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "agent_activity_stats payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::ActivityStats(payload)
        }
        FrameKind::AgentActivityChanged => {
            let payload: AgentActivityChangedPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::ActivityChanged(payload)
        }
        FrameKind::ContextCompactionNotify => {
            let payload: ContextCompactionNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "context_compaction_notify payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::ContextCompactionNotify(payload)
        }
        FrameKind::ContextCompactionCapability => {
            let payload: ContextCompactionCapabilityPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "context_compaction_capability payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::ContextCompactionCapability(payload)
        }
        FrameKind::ChatEvent => {
            let payload: ChatEvent = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::Chat(Box::new(payload))
        }
        FrameKind::SessionHistory => {
            let payload: SessionHistoryPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::SessionHistory(payload)
        }
        FrameKind::SessionSettings => {
            let payload: SessionSettingsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::SessionSettings(payload)
        }
        FrameKind::QueuedMessages => {
            let payload: QueuedMessagesPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            AgentEvent::QueuedMessages(payload)
        }
        kind => {
            return Err(ClientError::UnexpectedFrame {
                stream: envelope.stream,
                kind,
            });
        }
    };

    resolve_pending_agent_endpoint(
        shared,
        pending_agent_endpoints,
        &envelope.stream,
        agent_event_session_id(&event),
    )
    .await;
    let _ = tx.send(event).await;
    Ok(())
}

fn project_stream(project_id: &ProjectId) -> StreamPath {
    StreamPath(format!("/project/{}", project_id.0))
}

async fn handle_project_envelope(
    envelope: Envelope,
    shared: &Arc<Shared>,
) -> Result<(), ClientError> {
    let Some(RouteRef::Project(tx)) = shared.route_for(&envelope.stream) else {
        return Ok(());
    };

    let event = match envelope.kind {
        FrameKind::ProjectBootstrap => {
            let payload: ProjectBootstrapPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::Bootstrap(payload)
        }
        FrameKind::ProjectFileList => {
            let payload: ProjectFileListPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::FileList(payload)
        }
        FrameKind::ProjectGitStatus => {
            let payload: ProjectGitStatusPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::GitStatus(payload)
        }
        FrameKind::CodeIntelOverview => {
            let payload: CodeIntelOverviewPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelOverview(payload)
        }
        FrameKind::CodeIntelStatus => {
            let payload: CodeIntelStatusPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelStatus(payload)
        }
        FrameKind::CodeIntelFileModel => {
            let payload: CodeIntelFileModelPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelFileModel(payload)
        }
        FrameKind::CodeIntelDiagnostics => {
            let payload: CodeIntelDiagnosticsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelDiagnostics(payload)
        }
        FrameKind::CodeIntelHoverResult => {
            let payload: CodeIntelHoverResultPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelHoverResult(payload)
        }
        FrameKind::CodeIntelNavigateResult => {
            let payload: CodeIntelNavigateResultPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelNavigateResult(payload)
        }
        FrameKind::CodeIntelReferencesResults => {
            let payload: CodeIntelReferencesResultsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelReferencesResults(payload)
        }
        FrameKind::CodeIntelReferencesComplete => {
            let payload: CodeIntelReferencesCompletePayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelReferencesComplete(payload)
        }
        FrameKind::CodeIntelError => {
            let payload: CodeIntelErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::CodeIntelError(payload)
        }
        FrameKind::ProjectFileContents => {
            let payload: ProjectFileContentsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            assert!(
                !payload.path.relative_path.trim().is_empty(),
                "project_file_contents relative_path must not be empty"
            );
            ProjectEvent::FileContents(payload)
        }
        FrameKind::ProjectGitDiff => {
            let payload: ProjectGitDiffPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::GitDiff(payload)
        }
        FrameKind::ProjectEvent => {
            let payload: ProjectEventPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            ProjectEvent::Notify(payload)
        }
        kind => {
            return Err(ClientError::UnexpectedFrame {
                stream: envelope.stream,
                kind,
            });
        }
    };

    let _ = tx.send(event).await;
    Ok(())
}

async fn handle_terminal_envelope(
    envelope: Envelope,
    shared: &Arc<Shared>,
) -> Result<(), ClientError> {
    let Some(RouteRef::Terminal(tx)) = shared.route_for(&envelope.stream) else {
        return Ok(());
    };

    let event = match envelope.kind {
        FrameKind::TerminalBootstrap => {
            let payload: TerminalBootstrapPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            TerminalEvent::Bootstrap(payload)
        }
        FrameKind::TerminalStart => {
            let payload: TerminalStartPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            TerminalEvent::Start(payload)
        }
        FrameKind::TerminalOutput => {
            let payload: TerminalOutputPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            assert!(
                !payload.data.is_empty(),
                "terminal_output data must not be empty"
            );
            TerminalEvent::Output(payload)
        }
        FrameKind::TerminalExit => {
            let payload: TerminalExitPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            TerminalEvent::Exit(payload)
        }
        FrameKind::TerminalError => {
            let payload: TerminalErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(error) => return Err(ClientError::InvalidPayload(error)),
            };
            TerminalEvent::Error(payload)
        }
        kind => {
            return Err(ClientError::UnexpectedFrame {
                stream: envelope.stream,
                kind,
            });
        }
    };

    let _ = tx.send(event).await;
    Ok(())
}
