use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

mod runtime;

use protocol::{
    AgentErrorPayload, AgentId, AgentStartPayload, DumpSettingsPayload, Envelope, FrameError,
    FrameKind, HelloPayload, HostSettingsPayload, InterruptPayload, ListSessionsPayload,
    NewAgentPayload, NewTerminalPayload, PROTOCOL_VERSION, ProjectAddRootPayload,
    ProjectCreatePayload, ProjectDeletePayload, ProjectFileContentsPayload, ProjectFileListPayload,
    ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId, ProjectListDirPayload,
    ProjectNotifyPayload, ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRefreshPayload,
    ProjectRenamePayload, ProjectStageFilePayload, ProjectStageHunkPayload, RejectPayload,
    SendMessagePayload, SeqValidator, SessionListPayload, SetSettingPayload, SpawnAgentPayload,
    StreamPath, TYDE_VERSION, TerminalClosePayload, TerminalCreatePayload, TerminalErrorPayload,
    TerminalExitPayload, TerminalId, TerminalOutputPayload, TerminalResizePayload,
    TerminalSendPayload, TerminalStartPayload, Version, WelcomePayload, read_envelope,
    write_envelope,
};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use uuid::Uuid;

pub use runtime::{
    AgentCommands, AgentEndpoint, AgentEvent, AgentEvents, ClientError, HostCommands, HostEndpoint,
    HostEvent, HostEvents, ProjectCommands, ProjectEndpoint, ProjectEvent, ProjectEvents,
    TerminalCommands, TerminalEndpoint, TerminalEvent, TerminalEvents, connect_host_endpoint,
    connect_uds_host_endpoint,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ClientConfig {
    pub protocol_version: u32,
    pub tyde_version: Version,
    pub client_name: String,
    pub platform: String,
}

impl ClientConfig {
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            client_name: "tyde-desktop".to_owned(),
            platform: std::env::consts::OS.to_owned(),
        }
    }
}

pub struct Connection {
    pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub incoming_seq: SeqValidator,
    pub outgoing_seq: HashMap<StreamPath, u64>,
}

pub(crate) struct ConnectedParts {
    pub(crate) reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub(crate) writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub(crate) incoming_seq: SeqValidator,
    pub(crate) outgoing_seq: HashMap<StreamPath, u64>,
    pub(crate) host_stream: StreamPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stream {
    path: StreamPath,
}

impl Stream {
    pub fn from_path(path: StreamPath) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &StreamPath {
        &self.path
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("incoming_seq", &self.incoming_seq)
            .field("outgoing_seq", &self.outgoing_seq)
            .finish_non_exhaustive()
    }
}

impl Connection {
    pub async fn spawn_agent(&mut self, payload: SpawnAgentPayload) -> Result<(), FrameError> {
        let host_stream = self.host_stream();
        let seq = self
            .outgoing_seq
            .get(&host_stream)
            .copied()
            .expect("missing host stream sequence counter for spawn_agent");

        let envelope =
            Envelope::from_payload(host_stream.clone(), FrameKind::SpawnAgent, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(host_stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn list_sessions(&mut self, payload: ListSessionsPayload) -> Result<(), FrameError> {
        let host_stream = self.host_stream();
        let seq = self
            .outgoing_seq
            .get(&host_stream)
            .copied()
            .expect("missing host stream sequence counter for list_sessions");

        let envelope =
            Envelope::from_payload(host_stream.clone(), FrameKind::ListSessions, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(host_stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn dump_settings(&mut self, payload: DumpSettingsPayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::DumpSettings, &payload)
            .await
    }

    pub async fn set_setting(&mut self, payload: SetSettingPayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::SetSetting, &payload)
            .await
    }

    pub async fn project_create(
        &mut self,
        payload: ProjectCreatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectCreate, &payload)
            .await
    }

    pub async fn project_rename(
        &mut self,
        payload: ProjectRenamePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectRename, &payload)
            .await
    }

    pub async fn project_add_root(
        &mut self,
        payload: ProjectAddRootPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectAddRoot, &payload)
            .await
    }

    pub async fn project_delete(
        &mut self,
        payload: ProjectDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectDelete, &payload)
            .await
    }

    pub async fn project_refresh(&mut self, project_id: &ProjectId) -> Result<(), FrameError> {
        self.send_project_payload(
            project_id,
            FrameKind::ProjectRefresh,
            &ProjectRefreshPayload::default(),
        )
        .await
    }

    pub async fn project_read_file(
        &mut self,
        project_id: &ProjectId,
        payload: ProjectReadFilePayload,
    ) -> Result<(), FrameError> {
        self.send_project_payload(project_id, FrameKind::ProjectReadFile, &payload)
            .await
    }

    pub async fn project_read_diff(
        &mut self,
        project_id: &ProjectId,
        payload: ProjectReadDiffPayload,
    ) -> Result<(), FrameError> {
        self.send_project_payload(project_id, FrameKind::ProjectReadDiff, &payload)
            .await
    }

    pub async fn project_stage_file(
        &mut self,
        project_id: &ProjectId,
        payload: ProjectStageFilePayload,
    ) -> Result<(), FrameError> {
        self.send_project_payload(project_id, FrameKind::ProjectStageFile, &payload)
            .await
    }

    pub async fn project_stage_hunk(
        &mut self,
        project_id: &ProjectId,
        payload: ProjectStageHunkPayload,
    ) -> Result<(), FrameError> {
        self.send_project_payload(project_id, FrameKind::ProjectStageHunk, &payload)
            .await
    }

    pub async fn project_list_dir(
        &mut self,
        project_id: &ProjectId,
        payload: ProjectListDirPayload,
    ) -> Result<(), FrameError> {
        self.send_project_payload(project_id, FrameKind::ProjectListDir, &payload)
            .await
    }

    pub async fn terminal_create(
        &mut self,
        payload: TerminalCreatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TerminalCreate, &payload)
            .await
    }

    pub async fn terminal_send(
        &mut self,
        terminal_id: &TerminalId,
        payload: TerminalSendPayload,
    ) -> Result<(), FrameError> {
        self.send_terminal_payload(terminal_id, FrameKind::TerminalSend, &payload)
            .await
    }

    pub async fn terminal_resize(
        &mut self,
        terminal_id: &TerminalId,
        payload: TerminalResizePayload,
    ) -> Result<(), FrameError> {
        self.send_terminal_payload(terminal_id, FrameKind::TerminalResize, &payload)
            .await
    }

    pub async fn terminal_close(&mut self, terminal_id: &TerminalId) -> Result<(), FrameError> {
        self.send_terminal_payload(
            terminal_id,
            FrameKind::TerminalClose,
            &TerminalClosePayload::default(),
        )
        .await
    }

    pub async fn send_message(
        &mut self,
        stream: &StreamPath,
        message: String,
    ) -> Result<(), FrameError> {
        self.send_message_payload(
            stream,
            SendMessagePayload {
                message,
                images: None,
            },
        )
        .await
    }

    pub async fn send_message_payload(
        &mut self,
        stream: &StreamPath,
        payload: SendMessagePayload,
    ) -> Result<(), FrameError> {
        self.send_message_stream_payload(&Stream::from_path(stream.clone()), payload)
            .await
    }

    pub async fn send_message_stream(
        &mut self,
        stream: &Stream,
        message: String,
    ) -> Result<(), FrameError> {
        self.send_message_stream_payload(
            stream,
            SendMessagePayload {
                message,
                images: None,
            },
        )
        .await
    }

    pub async fn send_message_stream_payload(
        &mut self,
        stream: &Stream,
        payload: SendMessagePayload,
    ) -> Result<(), FrameError> {
        let seq = self
            .outgoing_seq
            .get(stream.path())
            .copied()
            .expect("send_message on unknown stream — AgentStart must be received first");
        let envelope =
            Envelope::from_payload(stream.path().clone(), FrameKind::SendMessage, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.path().clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn interrupt(&mut self, stream: &StreamPath) -> Result<(), FrameError> {
        self.interrupt_stream(&Stream::from_path(stream.clone()))
            .await
    }

    pub async fn interrupt_stream(&mut self, stream: &Stream) -> Result<(), FrameError> {
        let payload = InterruptPayload::default();
        let seq = self
            .outgoing_seq
            .get(stream.path())
            .copied()
            .expect("interrupt on unknown stream — AgentStart must be received first");
        let envelope =
            Envelope::from_payload(stream.path().clone(), FrameKind::Interrupt, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.path().clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn next_event(&mut self) -> Result<Option<Envelope>, FrameError> {
        let Some(envelope) = read_envelope(&mut self.reader).await? else {
            return Ok(None);
        };

        self.incoming_seq
            .validate(&envelope.stream, envelope.seq, envelope.kind);

        if envelope.stream.0.starts_with("/host/") {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&payload.instance_stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "new_agent payload agent_id {} does not match instance_stream {}",
                        payload.agent_id, payload.instance_stream
                    );
                    assert!(
                        !self.outgoing_seq.contains_key(&payload.instance_stream),
                        "duplicate NewAgent for stream {}",
                        payload.instance_stream
                    );
                    self.outgoing_seq.insert(payload.instance_stream, 0);
                }
                FrameKind::NewTerminal => {
                    let payload: NewTerminalPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_terminal_stream(&payload.stream);
                    assert_eq!(
                        payload.terminal_id, stream_parts.terminal_id,
                        "new_terminal payload terminal_id {} does not match stream {}",
                        payload.terminal_id, payload.stream
                    );
                    assert!(
                        !self.outgoing_seq.contains_key(&payload.stream),
                        "duplicate NewTerminal for stream {}",
                        payload.stream
                    );
                    self.outgoing_seq.insert(payload.stream, 0);
                }
                FrameKind::SessionList => {
                    let _: SessionListPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::ProjectNotify => {
                    let _: ProjectNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::HostSettings => {
                    let _: HostSettingsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on host stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/agent/") {
            match envelope.kind {
                FrameKind::AgentStart => {
                    assert_eq!(
                        envelope.seq, 0,
                        "AgentStart must be seq 0 on agent stream {}, got seq {}",
                        envelope.stream, envelope.seq
                    );
                    let payload: AgentStartPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "agent_start payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "AgentStart on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::AgentError => {
                    let payload: AgentErrorPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "agent_error payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "AgentError on stream {} before AgentStart",
                        envelope.stream
                    );
                }
                FrameKind::ChatEvent => {
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "ChatEvent on stream {} before AgentStart",
                        envelope.stream
                    );
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on agent stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/project/") {
            // The server proactively pushes project file list and git status
            // when projects are created, so the stream may not have been
            // initiated by the client yet.
            match envelope.kind {
                FrameKind::ProjectFileList => {
                    let _: ProjectFileListPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::ProjectGitStatus => {
                    let _: ProjectGitStatusPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::ProjectFileContents => {
                    let payload: ProjectFileContentsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    assert!(
                        !payload.path.relative_path.trim().is_empty(),
                        "project_file_contents relative_path must not be empty"
                    );
                }
                FrameKind::ProjectGitDiff => {
                    let _: ProjectGitDiffPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on project stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/terminal/") {
            assert!(
                self.outgoing_seq.contains_key(&envelope.stream),
                "server emitted terminal event on unknown stream {}",
                envelope.stream
            );
            match envelope.kind {
                FrameKind::TerminalStart => {
                    assert_eq!(
                        envelope.seq, 0,
                        "TerminalStart must be seq 0 on terminal stream {}, got seq {}",
                        envelope.stream, envelope.seq
                    );
                    let _: TerminalStartPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TerminalOutput => {
                    let payload: TerminalOutputPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    assert!(
                        !payload.data.is_empty(),
                        "terminal_output data must not be empty"
                    );
                }
                FrameKind::TerminalExit => {
                    let _: TerminalExitPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TerminalError => {
                    let _: TerminalErrorPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on terminal stream {}",
                        other, envelope.stream
                    );
                }
            }
        }

        Ok(Some(envelope))
    }

    fn host_stream(&self) -> StreamPath {
        let mut host_streams = self
            .outgoing_seq
            .keys()
            .filter(|stream| stream.0.starts_with("/host/"));

        let host_stream = host_streams
            .next()
            .cloned()
            .expect("missing /host/<uuid> stream in outgoing sequence map");
        assert!(
            host_streams.next().is_none(),
            "multiple host streams present in outgoing sequence map"
        );

        host_stream
    }

    async fn send_host_payload<T: serde::Serialize>(
        &mut self,
        kind: FrameKind,
        payload: &T,
    ) -> Result<(), FrameError> {
        let host_stream = self.host_stream();
        let seq = self
            .outgoing_seq
            .get(&host_stream)
            .copied()
            .expect("missing host stream sequence counter");

        let envelope = Envelope::from_payload(host_stream.clone(), kind, seq, payload)
            .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(host_stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    async fn send_project_payload<T: serde::Serialize>(
        &mut self,
        project_id: &ProjectId,
        kind: FrameKind,
        payload: &T,
    ) -> Result<(), FrameError> {
        let stream = self.project_stream(project_id);
        let seq = self
            .outgoing_seq
            .get(&stream)
            .copied()
            .expect("missing project stream sequence counter");

        let envelope =
            Envelope::from_payload(stream.clone(), kind, seq, payload).map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    async fn send_terminal_payload<T: serde::Serialize>(
        &mut self,
        terminal_id: &TerminalId,
        kind: FrameKind,
        payload: &T,
    ) -> Result<(), FrameError> {
        let stream = self.terminal_stream(terminal_id);
        let seq = self
            .outgoing_seq
            .get(&stream)
            .copied()
            .expect("missing terminal stream sequence counter");

        let envelope =
            Envelope::from_payload(stream.clone(), kind, seq, payload).map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    fn project_stream(&mut self, project_id: &ProjectId) -> StreamPath {
        let stream = StreamPath(format!("/project/{}", project_id));
        self.outgoing_seq.entry(stream.clone()).or_insert(0);
        stream
    }

    fn terminal_stream(&self, terminal_id: &TerminalId) -> StreamPath {
        StreamPath(format!("/terminal/{}", terminal_id))
    }
}

#[derive(Debug)]
pub(crate) struct AgentStreamParts {
    agent_id: AgentId,
}

pub(crate) fn parse_agent_stream(stream: &StreamPath) -> AgentStreamParts {
    let segments: Vec<&str> = stream.0.split('/').collect();
    assert_eq!(
        segments.len(),
        4,
        "agent stream must have format /agent/<agent_id>/<instance_id>, got {}",
        stream
    );
    assert!(
        segments.first() == Some(&""),
        "agent stream must be absolute path, got {}",
        stream
    );
    assert_eq!(
        segments[1], "agent",
        "expected /agent/<agent_id>/<instance_id> stream, got {}",
        stream
    );

    Uuid::parse_str(segments[2]).unwrap_or_else(|err| {
        panic!(
            "agent stream contains invalid agent_id UUID {} in {}: {}",
            segments[2], stream, err
        )
    });
    Uuid::parse_str(segments[3]).unwrap_or_else(|err| {
        panic!(
            "agent stream contains invalid instance_id UUID {} in {}: {}",
            segments[3], stream, err
        )
    });

    AgentStreamParts {
        agent_id: AgentId(segments[2].to_owned()),
    }
}

#[derive(Debug)]
pub(crate) struct TerminalStreamParts {
    terminal_id: TerminalId,
}

pub(crate) fn parse_terminal_stream(stream: &StreamPath) -> TerminalStreamParts {
    let segments: Vec<&str> = stream.0.split('/').collect();
    assert_eq!(
        segments.len(),
        3,
        "terminal stream must have format /terminal/<terminal_id>, got {}",
        stream
    );
    assert!(
        segments.first() == Some(&""),
        "terminal stream must be absolute path, got {}",
        stream
    );
    assert_eq!(
        segments[1], "terminal",
        "expected /terminal/<terminal_id> stream, got {}",
        stream
    );

    Uuid::parse_str(segments[2]).unwrap_or_else(|err| {
        panic!(
            "terminal stream contains invalid terminal_id UUID {} in {}: {}",
            segments[2], stream, err
        )
    });

    TerminalStreamParts {
        terminal_id: TerminalId(segments[2].to_owned()),
    }
}

#[derive(Debug)]
pub enum HandshakeError {
    Frame(FrameError),
    UnexpectedKind {
        expected: FrameKind,
        got: FrameKind,
    },
    StreamMismatch {
        expected: StreamPath,
        got: StreamPath,
    },
    IncompatibleProtocol {
        client: u32,
        server: u32,
    },
    InvalidPayload(serde_json::Error),
    Rejected(RejectPayload),
    Timeout,
}

impl From<FrameError> for HandshakeError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

pub async fn connect<S>(config: &ClientConfig, stream: S) -> Result<Connection, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let parts = connect_parts(config, stream).await?;

    Ok(Connection {
        reader: parts.reader,
        writer: parts.writer,
        incoming_seq: parts.incoming_seq,
        outgoing_seq: parts.outgoing_seq,
    })
}

pub async fn connect_uds(
    path: impl AsRef<Path>,
    config: &ClientConfig,
) -> Result<Connection, HandshakeError> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|err| HandshakeError::Frame(FrameError::Io(err)))?;
    connect(config, stream).await
}

pub(crate) async fn connect_parts<S>(
    config: &ClientConfig,
    stream: S,
) -> Result<ConnectedParts, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let stream_path = StreamPath(format!("/host/{}", Uuid::new_v4()));
    let hello = HelloPayload {
        protocol_version: config.protocol_version,
        tyde_version: config.tyde_version,
        client_name: config.client_name.clone(),
        platform: config.platform.clone(),
    };
    let hello_envelope = Envelope::from_payload(stream_path.clone(), FrameKind::Hello, 0, &hello)
        .map_err(HandshakeError::InvalidPayload)?;
    write_envelope(&mut write_half, &hello_envelope).await?;

    let response = timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut reader))
        .await
        .map_err(|_| HandshakeError::Timeout)??;

    let response = match response {
        Some(envelope) => envelope,
        None => {
            let io_err = io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed before handshake response",
            );
            return Err(HandshakeError::Frame(FrameError::Io(io_err)));
        }
    };

    let mut incoming_seq = SeqValidator::new();
    incoming_seq.validate(&response.stream, response.seq, response.kind);

    if response.stream != stream_path {
        return Err(HandshakeError::StreamMismatch {
            expected: stream_path,
            got: response.stream,
        });
    }

    match response.kind {
        FrameKind::Welcome => {
            let _welcome: WelcomePayload = response
                .parse_payload()
                .map_err(HandshakeError::InvalidPayload)?;

            let mut outgoing_seq = HashMap::new();
            outgoing_seq.insert(stream_path.clone(), 1);

            Ok(ConnectedParts {
                reader: Box::new(reader),
                writer: Box::new(write_half),
                incoming_seq,
                outgoing_seq,
                host_stream: stream_path,
            })
        }
        FrameKind::Reject => {
            let reject: RejectPayload = response
                .parse_payload()
                .map_err(HandshakeError::InvalidPayload)?;
            Err(HandshakeError::Rejected(reject))
        }
        got => Err(HandshakeError::UnexpectedKind {
            expected: FrameKind::Welcome,
            got,
        }),
    }
}
