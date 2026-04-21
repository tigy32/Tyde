use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use protocol::types::{AgentClosedPayload, CloseAgentPayload};
use protocol::{
    AgentErrorPayload, AgentRenamedPayload, AgentStartPayload, BackendSetupPayload, ChatEvent,
    CommandErrorPayload, CustomAgentNotifyPayload, Envelope, FrameError, FrameKind,
    HostSettingsPayload, InterruptPayload, ListSessionsPayload, McpServerNotifyPayload,
    NewAgentPayload, NewTerminalPayload, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectFileContentsPayload, ProjectFileListPayload,
    ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload,
    ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRefreshPayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectStageFilePayload, ProjectStageHunkPayload, QueuedMessagesPayload,
    SendMessagePayload, SessionListPayload, SessionSchemasPayload, SessionSettingsPayload,
    SetAgentNamePayload, SetSessionSettingsPayload, SkillNotifyPayload, SpawnAgentPayload,
    SteeringNotifyPayload, StreamPath, TerminalClosePayload, TerminalCreatePayload,
    TerminalErrorPayload, TerminalExitPayload, TerminalOutputPayload, TerminalResizePayload,
    TerminalSendPayload, TerminalStartPayload, read_envelope, write_envelope,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{
    ClientConfig, ConnectedParts, HandshakeError, connect_parts, parse_agent_stream,
    parse_terminal_stream,
};

const CHANNEL_CAPACITY: usize = 64;

#[derive(Debug)]
pub enum ClientError {
    Frame(FrameError),
    InvalidPayload(serde_json::Error),
    ConnectionClosed,
    StreamAlreadyRegistered(StreamPath),
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
    HostSettings(HostSettingsPayload),
    BackendSetup(BackendSetupPayload),
    SessionSchemas(SessionSchemasPayload),
    SessionList(SessionListPayload),
    CommandError(CommandErrorPayload),
    ProjectNotify(ProjectNotifyPayload),
    CustomAgentNotify(CustomAgentNotifyPayload),
    SteeringNotify(SteeringNotifyPayload),
    SkillNotify(SkillNotifyPayload),
    McpServerNotify(McpServerNotifyPayload),
    AgentClosed(AgentClosedPayload),
    NewAgent(AgentEndpoint),
    NewTerminal(TerminalEndpoint),
}

pub enum AgentEvent {
    Start(AgentStartPayload),
    Renamed(AgentRenamedPayload),
    Error(AgentErrorPayload),
    Chat(Box<ChatEvent>),
    SessionSettings(SessionSettingsPayload),
    QueuedMessages(QueuedMessagesPayload),
}

pub enum ProjectEvent {
    FileList(ProjectFileListPayload),
    GitStatus(ProjectGitStatusPayload),
    FileContents(ProjectFileContentsPayload),
    GitDiff(ProjectGitDiffPayload),
}

pub enum TerminalEvent {
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
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|err| HandshakeError::Frame(FrameError::Io(err)))?;
    connect_host_endpoint(config, stream).await
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

    pub async fn project_delete(&self, payload: ProjectDeletePayload) -> Result<(), ClientError> {
        self.send(FrameKind::ProjectDelete, &payload).await
    }

    pub async fn terminal_create(&self, payload: TerminalCreatePayload) -> Result<(), ClientError> {
        self.send(FrameKind::TerminalCreate, &payload).await
    }

    pub async fn open_project(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectEndpoint, ClientError> {
        let stream = StreamPath(format!("/project/{}", project_id.0));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        self.shared
            .register_route(stream.clone(), Route::Project(tx))?;
        self.shared
            .register_writable_stream(stream.clone(), 0)
            .await?;

        Ok(ProjectEndpoint {
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
        })
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
    pub async fn refresh(&self) -> Result<(), ClientError> {
        self.shared
            .send(
                self.stream.clone(),
                FrameKind::ProjectRefresh,
                &ProjectRefreshPayload::default(),
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
    write_tx: mpsc::Sender<WriteRequest>,
    _read_task: JoinHandle<()>,
    _write_task: JoinHandle<()>,
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

    let shared = Arc::new_cyclic(|weak| {
        let write_task = tokio::spawn(write_pump(parts.writer, parts.outgoing_seq, write_rx));
        let read_task = tokio::spawn(read_pump(
            parts.reader,
            parts.incoming_seq,
            parts.host_stream.clone(),
            host_tx.clone(),
            weak.clone(),
        ));

        let mut routes = HashMap::new();
        routes.insert(parts.host_stream.clone(), Route::Host);

        Shared {
            routes: Mutex::new(routes),
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
    mut reader: Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    mut incoming_seq: protocol::SeqValidator,
    host_stream: StreamPath,
    host_tx: mpsc::Sender<HostEvent>,
    shared: std::sync::Weak<Shared>,
) {
    while let Ok(Some(envelope)) = read_envelope(&mut reader).await {
        incoming_seq.validate(&envelope.stream, envelope.seq, envelope.kind);

        let Some(shared) = shared.upgrade() else {
            break;
        };

        if envelope.stream == host_stream {
            if !handle_host_envelope(envelope, host_tx.clone(), shared).await {
                break;
            }
            continue;
        }

        if envelope.stream.0.starts_with("/agent/") {
            handle_agent_envelope(envelope, &shared).await;
            continue;
        }

        if envelope.stream.0.starts_with("/project/") {
            handle_project_envelope(envelope, &shared).await;
            continue;
        }

        if envelope.stream.0.starts_with("/terminal/") {
            handle_terminal_envelope(envelope, &shared).await;
            continue;
        }
    }
}

async fn write_pump(
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    mut outgoing_seq: HashMap<StreamPath, u64>,
    mut rx: mpsc::Receiver<WriteRequest>,
) {
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
}

async fn handle_host_envelope(
    envelope: Envelope,
    host_tx: mpsc::Sender<HostEvent>,
    shared: Arc<Shared>,
) -> bool {
    match envelope.kind {
        FrameKind::HostSettings => {
            let payload: HostSettingsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::HostSettings(payload)).await;
            true
        }
        FrameKind::BackendSetup => {
            let payload: BackendSetupPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::BackendSetup(payload)).await;
            true
        }
        FrameKind::SessionList => {
            let payload: SessionListPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::SessionList(payload)).await;
            true
        }
        FrameKind::ProjectNotify => {
            let payload: ProjectNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::ProjectNotify(payload)).await;
            true
        }
        FrameKind::CustomAgentNotify => {
            let payload: CustomAgentNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::CustomAgentNotify(payload)).await;
            true
        }
        FrameKind::SteeringNotify => {
            let payload: SteeringNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::SteeringNotify(payload)).await;
            true
        }
        FrameKind::SkillNotify => {
            let payload: SkillNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::SkillNotify(payload)).await;
            true
        }
        FrameKind::McpServerNotify => {
            let payload: McpServerNotifyPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::McpServerNotify(payload)).await;
            true
        }
        FrameKind::SessionSchemas => {
            let payload: SessionSchemasPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::SessionSchemas(payload)).await;
            true
        }
        FrameKind::CommandError => {
            let payload: CommandErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::CommandError(payload)).await;
            true
        }
        FrameKind::AgentClosed => {
            let payload: AgentClosedPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
            let _ = host_tx.send(HostEvent::AgentClosed(payload)).await;
            true
        }
        FrameKind::NewAgent => {
            let payload: NewAgentPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
            };
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
                return false;
            }
            if shared
                .register_writable_stream(stream.clone(), 0)
                .await
                .is_err()
            {
                shared.unregister_route(&stream);
                return false;
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
                // Host event consumer dropped — unregister the route so the agent
                // stream doesn't accumulate buffered events with no reader.
                shared.unregister_route(&stream);
            }
            true
        }
        FrameKind::NewTerminal => {
            let payload: NewTerminalPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return false,
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
                return false;
            }
            if shared
                .register_writable_stream(stream.clone(), 0)
                .await
                .is_err()
            {
                shared.unregister_route(&stream);
                return false;
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
        other => panic!(
            "unexpected server frame kind {} on host stream {}",
            other, envelope.stream
        ),
    }
}

async fn handle_agent_envelope(envelope: Envelope, shared: &Arc<Shared>) {
    let Some(RouteRef::Agent(tx)) = shared.route_for(&envelope.stream) else {
        return;
    };

    let event = match envelope.kind {
        FrameKind::AgentStart => {
            assert_eq!(
                envelope.seq, 0,
                "AgentStart must be seq 0 on agent stream {}, got seq {}",
                envelope.stream, envelope.seq
            );
            let payload: AgentStartPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "agent_start payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::Start(payload)
        }
        FrameKind::AgentError => {
            let payload: AgentErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
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
                Err(_) => return,
            };
            let stream_parts = parse_agent_stream(&envelope.stream);
            assert_eq!(
                payload.agent_id, stream_parts.agent_id,
                "agent_renamed payload agent_id {} does not match stream {}",
                payload.agent_id, envelope.stream
            );
            AgentEvent::Renamed(payload)
        }
        FrameKind::ChatEvent => {
            let payload: ChatEvent = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            AgentEvent::Chat(Box::new(payload))
        }
        FrameKind::SessionSettings => {
            let payload: SessionSettingsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            AgentEvent::SessionSettings(payload)
        }
        FrameKind::QueuedMessages => {
            let payload: QueuedMessagesPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            AgentEvent::QueuedMessages(payload)
        }
        other => panic!(
            "unexpected server frame kind {} on agent stream {}",
            other, envelope.stream
        ),
    };

    let _ = tx.send(event).await;
}

async fn handle_project_envelope(envelope: Envelope, shared: &Arc<Shared>) {
    let Some(RouteRef::Project(tx)) = shared.route_for(&envelope.stream) else {
        return;
    };

    let event = match envelope.kind {
        FrameKind::ProjectFileList => {
            let payload: ProjectFileListPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            ProjectEvent::FileList(payload)
        }
        FrameKind::ProjectGitStatus => {
            let payload: ProjectGitStatusPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            ProjectEvent::GitStatus(payload)
        }
        FrameKind::ProjectFileContents => {
            let payload: ProjectFileContentsPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
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
                Err(_) => return,
            };
            ProjectEvent::GitDiff(payload)
        }
        other => panic!(
            "unexpected server frame kind {} on project stream {}",
            other, envelope.stream
        ),
    };

    let _ = tx.send(event).await;
}

async fn handle_terminal_envelope(envelope: Envelope, shared: &Arc<Shared>) {
    let Some(RouteRef::Terminal(tx)) = shared.route_for(&envelope.stream) else {
        return;
    };

    let event = match envelope.kind {
        FrameKind::TerminalStart => {
            assert_eq!(
                envelope.seq, 0,
                "TerminalStart must be seq 0 on terminal stream {}, got seq {}",
                envelope.stream, envelope.seq
            );
            let payload: TerminalStartPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            TerminalEvent::Start(payload)
        }
        FrameKind::TerminalOutput => {
            let payload: TerminalOutputPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
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
                Err(_) => return,
            };
            TerminalEvent::Exit(payload)
        }
        FrameKind::TerminalError => {
            let payload: TerminalErrorPayload = match envelope.parse_payload() {
                Ok(payload) => payload,
                Err(_) => return,
            };
            TerminalEvent::Error(payload)
        }
        other => panic!(
            "unexpected server frame kind {} on terminal stream {}",
            other, envelope.stream
        ),
    };

    let _ = tx.send(event).await;
}
