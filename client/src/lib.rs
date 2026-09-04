use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

mod runtime;

use protocol::types::{AgentClosedPayload, CloseAgentPayload};
use protocol::{
    AgentActivityStatsPayload, AgentActivitySummaryPayload, AgentBootstrapPayload,
    AgentErrorPayload, AgentId, AgentRenamedPayload, AgentStartPayload,
    AgentTurnStateNotifyPayload, AgentsViewPreferencesNotifyPayload, BackendCapacityPayload,
    BackendConfigSchemasPayload, BackendConfigSnapshotsPayload, BackendSettingsRefreshPayload,
    BackendSetupPayload, BrowseBootstrapPayload, CancelBackgroundTaskPayload,
    CancelQueuedMessagePayload, CancelWorkflowPayload, CodeIntelDiagnosticsPayload,
    CodeIntelErrorPayload, CodeIntelFileModelPayload, CodeIntelHoverResultPayload,
    CodeIntelNavigateResultPayload, CodeIntelOverviewPayload, CodeIntelReferencesCompletePayload,
    CodeIntelReferencesResultsPayload, CodeIntelStatusPayload, CommandErrorPayload,
    ContextCompactionCapabilityPayload, ContextCompactionNotifyPayload, CustomAgentDeletePayload,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, DeleteSessionPayload,
    EditQueuedMessagePayload, Envelope, FetchSessionHistoryPayload, FrameError, FrameKind,
    FrameReader, HelloPayload, HostBootstrapPayload, HostBrowseStartPayload, HostSettingsPayload,
    InterruptPayload, LaunchProfileCatalogPayload, ListSessionsPayload, McpServerDeletePayload,
    McpServerNotifyPayload, McpServerUpsertPayload, MobileAccessStatePayload,
    MobilePairingOfferPayload, MoveSessionPayload, NewAgentPayload, NewTerminalPayload,
    PROTOCOL_VERSION, ProjectAccessedPayload, ProjectAddRootPayload, ProjectBootstrapPayload,
    ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectId, ProjectListDirPayload, ProjectNotifyPayload,
    ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRenamePayload, ProjectReorderPayload,
    ProjectStageFilePayload, ProjectStageHunkPayload, ProtocolFrame, QueuedMessagesPayload,
    RejectPayload, ReviewActionPayload, ReviewBootstrapPayload, ReviewCreatePayload,
    ReviewEventPayload, ReviewId, ReviewSubscribePayload, SendMessagePayload,
    SendQueuedMessageNowPayload, SeqValidator, SessionHistoryPayload, SessionListPayload,
    SessionSchemasPayload, SessionSettingsPayload, SessionSummaryCountUpdatedPayload,
    SetAgentNamePayload, SetSessionSettingsPayload, SkillNotifyPayload, SkillRefreshPayload,
    SpawnAgentPayload, SteeringDeletePayload, SteeringNotifyPayload, SteeringUpsertPayload,
    StreamPath, TYDE_VERSION, TaskTokenUsagePayload, TeamCompactPayload,
    TeamContextCompactionNotifyPayload, TeamCreatePayload, TeamDeletePayload,
    TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftDiscardPayload, TeamDraftNotifyPayload, TeamDraftShufflePayload,
    TeamDraftUpdatePayload, TeamMemberActivatePayload, TeamMemberBindingNotifyPayload,
    TeamMemberCreatePayload, TeamMemberDeletePayload, TeamMemberNotifyPayload,
    TeamMemberShufflePayload, TeamMemberUpdatePayload, TeamNotifyPayload,
    TeamPresetCatalogNotifyPayload, TeamRenamePayload, TeamSetManagerPayload,
    TerminalBootstrapPayload, TerminalClosePayload, TerminalCreatePayload, TerminalErrorPayload,
    TerminalExitPayload, TerminalId, TerminalOutputPayload, TerminalResizePayload,
    TerminalSendPayload, TerminalStartPayload, TriggerWorkflowPayload, Version, WelcomePayload,
    WorkbenchCreatePayload, WorkbenchRemovePayload, WorkflowNotifyPayload, WorkflowRefreshPayload,
    WorkflowRunNotifyPayload, read_envelope, write_envelope,
};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, BufReader};
#[cfg(unix)]
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
    pub reader: FrameReader<Box<dyn AsyncBufRead + Unpin + Send>>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub incoming_seq: SeqValidator,
    pub outgoing_seq: HashMap<StreamPath, u64>,
    voice_state: ServerVoiceState,
}

pub(crate) struct ConnectedParts {
    pub(crate) reader: FrameReader<Box<dyn AsyncBufRead + Unpin + Send>>,
    pub(crate) writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub(crate) incoming_seq: SeqValidator,
    pub(crate) outgoing_seq: HashMap<StreamPath, u64>,
    pub(crate) host_stream: StreamPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stream {
    path: StreamPath,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VoiceEvent {
    Capabilities(protocol::VoiceCapabilitiesPayload),
    Accepted(protocol::VoiceAcceptedPayload),
    Audio {
        payload: protocol::VoiceAudioPayload,
        opus: Vec<u8>,
    },
    Transcript(protocol::VoiceTranscriptPayload),
    State(protocol::VoiceStatePayload),
    Output(protocol::VoiceSessionPayload),
    Stop(protocol::VoiceStopPayload),
    Error(protocol::VoiceErrorPayload),
}

#[derive(Default)]
struct ServerVoiceState {
    active: Option<ServerVoiceSession>,
    last_generation: u64,
}

struct ServerVoiceSession {
    session_id: protocol::VoiceSessionId,
    generation: u64,
    mode: protocol::VoiceMode,
    next_output_media_seq: u64,
}

fn is_voice_frame(frame: &ProtocolFrame) -> bool {
    frame.envelope.stream.0.starts_with("/voice")
        || matches!(
            frame.envelope.kind,
            FrameKind::VoiceCapabilities
                | FrameKind::VoiceAccepted
                | FrameKind::VoiceAudio
                | FrameKind::VoiceTranscript
                | FrameKind::VoiceState
                | FrameKind::VoiceOutput
                | FrameKind::VoiceStop
                | FrameKind::VoiceError
        )
}

fn reject_voice(message: impl Into<String>) -> FrameError {
    FrameError::Protocol(message.into())
}

fn validate_capabilities(
    capabilities: &protocol::VoiceCapabilitiesPayload,
) -> Result<(), FrameError> {
    if !capabilities.valid() {
        return Err(reject_voice("invalid server voice capabilities"));
    }
    Ok(())
}

fn parse_voice_payload<T: serde::de::DeserializeOwned>(
    frame: &ProtocolFrame,
    name: &str,
) -> Result<T, FrameError> {
    frame
        .envelope
        .parse_payload()
        .map_err(|error| reject_voice(format!("invalid {name}: {error}")))
}

fn require_empty_voice_body(frame: &ProtocolFrame) -> Result<(), FrameError> {
    if frame.binary.is_empty() {
        Ok(())
    } else {
        Err(reject_voice(format!(
            "{} must not carry binary data",
            frame.envelope.kind
        )))
    }
}

fn require_active_voice<'a>(
    state: &'a ServerVoiceState,
    frame: &ProtocolFrame,
    session_id: &protocol::VoiceSessionId,
    generation: u64,
) -> Result<&'a ServerVoiceSession, FrameError> {
    require_active_voice_on(
        state,
        frame,
        session_id,
        generation,
        StreamPath(format!("/voice/{}", session_id.0)),
    )
}

fn require_active_voice_on<'a>(
    state: &'a ServerVoiceState,
    frame: &ProtocolFrame,
    session_id: &protocol::VoiceSessionId,
    generation: u64,
    expected_stream: StreamPath,
) -> Result<&'a ServerVoiceSession, FrameError> {
    state
        .active
        .as_ref()
        .filter(|active| {
            active.session_id == *session_id
                && active.generation == generation
                && frame.envelope.stream == expected_stream
        })
        .ok_or_else(|| reject_voice("stale, foreign, or misrouted server voice frame"))
}

fn accept_server_voice_frame(
    state: &mut ServerVoiceState,
    host_stream: &StreamPath,
    frame: &ProtocolFrame,
) -> Result<Option<VoiceEvent>, FrameError> {
    if !is_voice_frame(frame) {
        return Ok(None);
    }
    match frame.envelope.kind {
        FrameKind::VoiceCapabilities => {
            require_empty_voice_body(frame)?;
            if &frame.envelope.stream != host_stream {
                return Err(reject_voice(
                    "server VoiceCapabilities must use the connected host stream",
                ));
            }
            let payload = parse_voice_payload(frame, "VoiceCapabilities")?;
            validate_capabilities(&payload)?;
            Ok(Some(VoiceEvent::Capabilities(payload)))
        }
        FrameKind::VoiceAccepted => {
            require_empty_voice_body(frame)?;
            if frame.envelope.stream.0 != "/voice" {
                return Err(reject_voice("server VoiceAccepted must use /voice"));
            }
            let payload: protocol::VoiceAcceptedPayload =
                parse_voice_payload(frame, "VoiceAccepted")?;
            if payload.session_id.0.is_empty()
                || payload.generation <= state.last_generation
                || !payload.request.valid()
            {
                return Err(reject_voice("invalid server VoiceAccepted session"));
            }
            state.last_generation = payload.generation;
            state.active = Some(ServerVoiceSession {
                session_id: payload.session_id.clone(),
                generation: payload.generation,
                mode: payload.request.mode(),
                next_output_media_seq: 0,
            });
            Ok(Some(VoiceEvent::Accepted(payload)))
        }
        FrameKind::VoiceAudio => {
            let payload: protocol::VoiceAudioPayload = parse_voice_payload(frame, "VoiceAudio")?;
            payload
                .validate_body(frame.binary.len())
                .map_err(reject_voice)?;
            if payload.direction != protocol::VoiceDirection::Output
                || payload.timestamp_samples_48k != payload.first_media_seq.saturating_mul(960)
            {
                return Err(reject_voice(
                    "invalid server VoiceAudio direction or timestamp",
                ));
            }
            // Downlink audio rides its own sub-stream with its own envelope
            // sequence counter. The shell consumes audio natively while the
            // frontend validates the JSON envelope stream, so sharing one
            // counter desyncs the frontend (a partial observer) at Nova's
            // first spoken word.
            let active = require_active_voice_on(
                state,
                frame,
                &payload.session_id,
                payload.generation,
                StreamPath(format!("/voice/{}/audio", payload.session_id.0)),
            )?;
            if active.mode == protocol::VoiceMode::Dictation {
                return Err(reject_voice("dictation received output audio"));
            }
            if payload.first_media_seq < active.next_output_media_seq {
                return Err(reject_voice("server voice media sequence moved backwards"));
            }
            let next = payload.first_media_seq + payload.packet_lengths.len() as u64;
            state
                .active
                .as_mut()
                .expect("validated active voice session")
                .next_output_media_seq = next;
            Ok(Some(VoiceEvent::Audio {
                payload,
                opus: frame.binary.clone(),
            }))
        }
        FrameKind::VoiceTranscript => {
            require_empty_voice_body(frame)?;
            let payload: protocol::VoiceTranscriptPayload =
                parse_voice_payload(frame, "VoiceTranscript")?;
            let active =
                require_active_voice(state, frame, &payload.session_id, payload.generation)?;
            if active.mode == protocol::VoiceMode::Dictation
                && (payload.speaker != protocol::VoiceTranscriptSpeaker::User
                    || payload.message_id.is_some())
            {
                return Err(reject_voice("dictation received non-provider transcript"));
            }
            Ok(Some(VoiceEvent::Transcript(payload)))
        }
        FrameKind::VoiceState => {
            require_empty_voice_body(frame)?;
            let payload: protocol::VoiceStatePayload = parse_voice_payload(frame, "VoiceState")?;
            let active =
                require_active_voice(state, frame, &payload.session_id, payload.generation)?;
            if active.mode == protocol::VoiceMode::Dictation
                && !matches!(
                    payload.state,
                    protocol::VoiceSessionState::Starting
                        | protocol::VoiceSessionState::Listening
                        | protocol::VoiceSessionState::Ending
                        | protocol::VoiceSessionState::Ended
                )
            {
                return Err(reject_voice("dictation received conversation state"));
            }
            Ok(Some(VoiceEvent::State(payload)))
        }
        FrameKind::VoiceOutput => {
            require_empty_voice_body(frame)?;
            let payload: protocol::VoiceSessionPayload = parse_voice_payload(frame, "VoiceOutput")?;
            let active =
                require_active_voice(state, frame, &payload.session_id, payload.generation)?;
            if active.mode == protocol::VoiceMode::Dictation {
                return Err(reject_voice("dictation received VoiceOutput"));
            }
            Ok(Some(VoiceEvent::Output(payload)))
        }
        FrameKind::VoiceStop => {
            require_empty_voice_body(frame)?;
            let payload: protocol::VoiceStopPayload = parse_voice_payload(frame, "VoiceStop")?;
            require_active_voice(state, frame, &payload.session_id, payload.generation)?;
            state.active = None;
            Ok(Some(VoiceEvent::Stop(payload)))
        }
        FrameKind::VoiceError => {
            require_empty_voice_body(frame)?;
            let payload: protocol::VoiceErrorPayload = parse_voice_payload(frame, "VoiceError")?;
            if let Some(session_id) = &payload.session_id {
                require_active_voice(state, frame, session_id, payload.generation)?;
                if payload.fatal {
                    state.active = None;
                }
            } else if frame.envelope.stream.0 != "/voice" {
                return Err(reject_voice("pre-session VoiceError must use /voice"));
            }
            Ok(Some(VoiceEvent::Error(payload)))
        }
        other => Err(reject_voice(format!(
            "unexpected {other} on server voice stream"
        ))),
    }
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

    /// Registers this device's Web Push subscription with the host. Valid only
    /// on a mobile connection: the host binds it to the device the transport
    /// authenticated as, ignoring anything the client might claim.
    pub async fn mobile_push_subscribe(
        &mut self,
        payload: protocol::MobilePushSubscribePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::MobilePushSubscribe, &payload)
            .await
    }

    pub async fn mobile_push_unsubscribe(
        &mut self,
        payload: protocol::MobilePushUnsubscribePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::MobilePushUnsubscribe, &payload)
            .await
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

    pub async fn host_browse_start(
        &mut self,
        payload: HostBrowseStartPayload,
    ) -> Result<(), FrameError> {
        let browse_stream = payload.browse_stream.clone();
        assert!(
            !self.outgoing_seq.contains_key(&browse_stream),
            "duplicate browse stream {}",
            browse_stream
        );
        self.send_host_payload(FrameKind::HostBrowseStart, &payload)
            .await?;
        self.outgoing_seq.insert(browse_stream, 0);
        Ok(())
    }

    pub async fn delete_session(
        &mut self,
        payload: DeleteSessionPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::DeleteSession, &payload)
            .await
    }

    pub async fn move_session(&mut self, payload: MoveSessionPayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::MoveSession, &payload)
            .await
    }

    pub async fn settings_write(
        &mut self,
        payload: protocol::SettingsWritePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::SettingsWrite, &payload)
            .await
    }

    pub async fn replace_setting<V: serde::Serialize, E: serde::Serialize>(
        &mut self,
        path: impl Into<String>,
        value: V,
        expected: E,
    ) -> Result<protocol::SettingsWriteId, FrameError> {
        let write_id = protocol::SettingsWriteId(Uuid::new_v4().to_string());
        self.settings_write(protocol::SettingsWritePayload {
            write_id: write_id.clone(),
            ops: vec![protocol::SettingOp::Replace {
                path: path.into(),
                value: serde_json::to_value(value).map_err(FrameError::Json)?,
                expected: protocol::SettingExpectation::Value {
                    value: serde_json::to_value(expected).map_err(FrameError::Json)?,
                },
            }],
        })
        .await?;
        Ok(write_id)
    }

    pub async fn backend_native_settings_write(
        &mut self,
        backend: protocol::BackendKind,
        settings: impl serde::Serialize,
    ) -> Result<protocol::SettingsWriteId, FrameError> {
        let write_id = protocol::SettingsWriteId(Uuid::new_v4().to_string());
        self.send_host_payload(
            FrameKind::BackendNativeSettingsWrite,
            &protocol::BackendNativeSettingsWritePayload {
                write_id: write_id.clone(),
                backend,
                settings: serde_json::to_value(settings).map_err(FrameError::Json)?,
            },
        )
        .await?;
        Ok(write_id)
    }

    pub async fn invoke_settings_action(
        &mut self,
        backend: protocol::BackendKind,
        resource: impl Into<String>,
        action: impl Into<String>,
        arguments: impl serde::Serialize,
    ) -> Result<protocol::SettingsWriteId, FrameError> {
        let write_id = protocol::SettingsWriteId(Uuid::new_v4().to_string());
        self.send_host_payload(
            FrameKind::InvokeSettingsAction,
            &protocol::InvokeSettingsActionPayload {
                write_id: write_id.clone(),
                backend,
                resource: resource.into(),
                action: action.into(),
                arguments: serde_json::to_value(arguments).map_err(FrameError::Json)?,
            },
        )
        .await?;
        Ok(write_id)
    }

    pub async fn custom_agent_upsert(
        &mut self,
        payload: CustomAgentUpsertPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::CustomAgentUpsert, &payload)
            .await
    }

    pub async fn custom_agent_delete(
        &mut self,
        payload: CustomAgentDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::CustomAgentDelete, &payload)
            .await
    }

    pub async fn steering_upsert(
        &mut self,
        payload: SteeringUpsertPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::SteeringUpsert, &payload)
            .await
    }

    pub async fn steering_delete(
        &mut self,
        payload: SteeringDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::SteeringDelete, &payload)
            .await
    }

    pub async fn skill_refresh(&mut self, payload: SkillRefreshPayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::SkillRefresh, &payload)
            .await
    }

    pub async fn backend_settings_refresh(
        &mut self,
        payload: BackendSettingsRefreshPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::BackendSettingsRefresh, &payload)
            .await
    }

    pub async fn mcp_server_upsert(
        &mut self,
        payload: McpServerUpsertPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::McpServerUpsert, &payload)
            .await
    }

    pub async fn mcp_server_delete(
        &mut self,
        payload: McpServerDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::McpServerDelete, &payload)
            .await
    }

    pub async fn workflow_refresh(
        &mut self,
        payload: WorkflowRefreshPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::WorkflowRefresh, &payload)
            .await
    }

    pub async fn trigger_workflow(
        &mut self,
        payload: TriggerWorkflowPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TriggerWorkflow, &payload)
            .await
    }

    pub async fn cancel_workflow(
        &mut self,
        payload: CancelWorkflowPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::CancelWorkflow, &payload)
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

    pub async fn project_reorder(
        &mut self,
        payload: ProjectReorderPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectReorder, &payload)
            .await
    }

    pub async fn project_add_root(
        &mut self,
        payload: ProjectAddRootPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectAddRoot, &payload)
            .await
    }

    pub async fn project_delete_root(
        &mut self,
        payload: ProjectDeleteRootPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectDeleteRoot, &payload)
            .await
    }

    pub async fn project_delete(
        &mut self,
        payload: ProjectDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::ProjectDelete, &payload)
            .await
    }

    pub async fn workbench_create(
        &mut self,
        payload: WorkbenchCreatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::WorkbenchCreate, &payload)
            .await
    }

    pub async fn workbench_remove(
        &mut self,
        payload: WorkbenchRemovePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::WorkbenchRemove, &payload)
            .await
    }

    pub async fn team_create(&mut self, payload: TeamCreatePayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamCreate, &payload)
            .await
    }

    pub async fn team_compact(&mut self, payload: TeamCompactPayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamCompact, &payload)
            .await
    }

    pub async fn team_rename(&mut self, payload: TeamRenamePayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamRename, &payload)
            .await
    }

    pub async fn team_delete(&mut self, payload: TeamDeletePayload) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDelete, &payload)
            .await
    }

    pub async fn team_set_manager(
        &mut self,
        payload: TeamSetManagerPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamSetManager, &payload)
            .await
    }

    pub async fn team_member_create(
        &mut self,
        payload: TeamMemberCreatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamMemberCreate, &payload)
            .await
    }

    pub async fn team_member_update(
        &mut self,
        payload: TeamMemberUpdatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamMemberUpdate, &payload)
            .await
    }

    pub async fn team_member_delete(
        &mut self,
        payload: TeamMemberDeletePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamMemberDelete, &payload)
            .await
    }

    pub async fn team_member_activate(
        &mut self,
        payload: TeamMemberActivatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamMemberActivate, &payload)
            .await
    }

    pub async fn team_draft_create(
        &mut self,
        payload: TeamDraftCreatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftCreate, &payload)
            .await
    }

    pub async fn team_draft_update(
        &mut self,
        payload: TeamDraftUpdatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftUpdate, &payload)
            .await
    }

    pub async fn team_draft_shuffle(
        &mut self,
        payload: TeamDraftShufflePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftShuffle, &payload)
            .await
    }

    pub async fn team_member_shuffle(
        &mut self,
        payload: TeamMemberShufflePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamMemberShuffle, &payload)
            .await
    }

    pub async fn team_draft_apply_template(
        &mut self,
        payload: TeamDraftApplyTemplatePayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftApplyTemplate, &payload)
            .await
    }

    pub async fn team_draft_commit(
        &mut self,
        payload: TeamDraftCommitPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftCommit, &payload)
            .await
    }

    pub async fn team_draft_discard(
        &mut self,
        payload: TeamDraftDiscardPayload,
    ) -> Result<(), FrameError> {
        self.send_host_payload(FrameKind::TeamDraftDiscard, &payload)
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

    pub async fn project_accessed(&mut self, project_id: &ProjectId) -> Result<(), FrameError> {
        self.send_project_payload(
            project_id,
            FrameKind::ProjectAccessed,
            &ProjectAccessedPayload::default(),
        )
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

    pub async fn review_create(
        &mut self,
        project_id: &ProjectId,
        payload: ReviewCreatePayload,
    ) -> Result<(), FrameError> {
        let stream = self.project_stream(project_id);
        let seq = self
            .outgoing_seq
            .get(&stream)
            .copied()
            .expect("missing project stream sequence counter");
        let selection_kind = payload.selection.kind_name();
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::ReviewCreate, seq, &payload)
                .map_err(FrameError::Json)?;
        tracing::debug!(
            project_id = %project_id,
            selection_kind,
            stream = %stream,
            seq,
            "sending review_create"
        );
        self.outgoing_seq.insert(stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn review_action(
        &mut self,
        review_id: &ReviewId,
        payload: ReviewActionPayload,
    ) -> Result<(), FrameError> {
        let stream = self.review_stream(review_id);
        let seq = self
            .outgoing_seq
            .get(&stream)
            .copied()
            .expect("missing review stream sequence counter");
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::ReviewAction, seq, &payload)
                .map_err(FrameError::Json)?;
        tracing::debug!(
            review_id = %review_id,
            action_kind = payload.kind_name(),
            stream = %stream,
            seq,
            "sending review_action"
        );
        self.outgoing_seq.insert(stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn review_subscribe(
        &mut self,
        review_id: &ReviewId,
        payload: ReviewSubscribePayload,
    ) -> Result<(), FrameError> {
        let stream = self.review_stream(review_id);
        let seq = self
            .outgoing_seq
            .get(&stream)
            .copied()
            .expect("missing review stream sequence counter");
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::ReviewSubscribe, seq, &payload)
                .map_err(FrameError::Json)?;
        tracing::debug!(
            review_id = %review_id,
            stream = %stream,
            seq,
            "sending review_subscribe"
        );
        self.outgoing_seq.insert(stream, seq + 1);
        write_envelope(&mut self.writer, &envelope).await
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
                origin: None,
                tool_response: None,
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
                origin: None,
                tool_response: None,
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

    pub async fn fetch_session_history(
        &mut self,
        stream: &StreamPath,
        payload: FetchSessionHistoryPayload,
    ) -> Result<(), FrameError> {
        let seq =
            self.outgoing_seq.get(stream).copied().expect(
                "fetch_session_history on unknown stream — AgentStart must be received first",
            );
        let envelope = Envelope::from_payload(
            stream.clone(),
            FrameKind::FetchSessionHistory,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn set_agent_name(
        &mut self,
        stream: &StreamPath,
        name: String,
    ) -> Result<(), FrameError> {
        self.set_agent_name_stream(&Stream::from_path(stream.clone()), name)
            .await
    }

    pub async fn compact_agent(
        &mut self,
        stream: &StreamPath,
        payload: protocol::AgentCompactPayload,
    ) -> Result<(), FrameError> {
        let seq = self
            .outgoing_seq
            .get(stream)
            .copied()
            .expect("compact_agent on unknown stream — AgentStart must be received first");
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::AgentCompact, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn set_agent_name_stream(
        &mut self,
        stream: &Stream,
        name: String,
    ) -> Result<(), FrameError> {
        let payload = SetAgentNamePayload { name };
        let seq = self
            .outgoing_seq
            .get(stream.path())
            .copied()
            .expect("set_agent_name on unknown stream — AgentStart must be received first");
        let envelope = Envelope::from_payload(
            stream.path().clone(),
            FrameKind::SetAgentName,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.path().clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn set_session_settings(
        &mut self,
        stream: &StreamPath,
        payload: SetSessionSettingsPayload,
    ) -> Result<(), FrameError> {
        let stream = Stream::from_path(stream.clone());
        let seq =
            self.outgoing_seq.get(stream.path()).copied().expect(
                "set_session_settings on unknown stream — AgentStart must be received first",
            );
        let envelope = Envelope::from_payload(
            stream.path().clone(),
            FrameKind::SetSessionSettings,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.path().clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn interrupt(&mut self, stream: &StreamPath) -> Result<(), FrameError> {
        self.interrupt_stream(&Stream::from_path(stream.clone()))
            .await
    }

    pub async fn close_agent(&mut self, stream: &StreamPath) -> Result<(), FrameError> {
        self.close_agent_stream(&Stream::from_path(stream.clone()))
            .await
    }

    pub async fn close_agent_stream(&mut self, stream: &Stream) -> Result<(), FrameError> {
        let payload = CloseAgentPayload::default();
        let seq = self
            .outgoing_seq
            .get(stream.path())
            .copied()
            .expect("close_agent on unknown stream — AgentStart must be received first");
        let envelope =
            Envelope::from_payload(stream.path().clone(), FrameKind::CloseAgent, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.path().clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
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

    pub async fn cancel_background_task(
        &mut self,
        stream: &StreamPath,
        payload: CancelBackgroundTaskPayload,
    ) -> Result<(), FrameError> {
        let seq =
            self.outgoing_seq.get(stream).copied().expect(
                "cancel_background_task on unknown stream — AgentStart must be received first",
            );
        let envelope = Envelope::from_payload(
            stream.clone(),
            FrameKind::CancelBackgroundTask,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn cancel_queued_message(
        &mut self,
        stream: &StreamPath,
        payload: CancelQueuedMessagePayload,
    ) -> Result<(), FrameError> {
        let seq =
            self.outgoing_seq.get(stream).copied().expect(
                "cancel_queued_message on unknown stream — AgentStart must be received first",
            );
        let envelope = Envelope::from_payload(
            stream.clone(),
            FrameKind::CancelQueuedMessage,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn edit_queued_message(
        &mut self,
        stream: &StreamPath,
        payload: EditQueuedMessagePayload,
    ) -> Result<(), FrameError> {
        let seq =
            self.outgoing_seq.get(stream).copied().expect(
                "edit_queued_message on unknown stream — AgentStart must be received first",
            );
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::EditQueuedMessage, seq, &payload)
                .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn send_queued_message_now(
        &mut self,
        stream: &StreamPath,
        payload: SendQueuedMessageNowPayload,
    ) -> Result<(), FrameError> {
        let seq = self.outgoing_seq.get(stream).copied().expect(
            "send_queued_message_now on unknown stream — AgentStart must be received first",
        );
        let envelope = Envelope::from_payload(
            stream.clone(),
            FrameKind::SendQueuedMessageNow,
            seq,
            &payload,
        )
        .map_err(FrameError::Json)?;
        self.outgoing_seq.insert(stream.clone(), seq + 1);
        write_envelope(&mut self.writer, &envelope).await
    }

    pub async fn next_event(&mut self) -> Result<Option<Envelope>, FrameError> {
        loop {
            let Some(frame) = self.next_frame().await? else {
                return Ok(None);
            };
            if is_voice_frame(&frame) {
                continue;
            }
            if !frame.binary.is_empty() {
                return Err(FrameError::Protocol(
                    "binary frame requires next_frame".into(),
                ));
            }
            return Ok(Some(frame.envelope));
        }
    }

    pub async fn next_frame(&mut self) -> Result<Option<ProtocolFrame>, FrameError> {
        let Some(frame) = self.reader.read_frame().await? else {
            return Ok(None);
        };
        let envelope = &frame.envelope;

        self.incoming_seq
            .validate(&envelope.stream, envelope.seq, envelope.kind)?;

        let host_stream = self.host_stream();
        if accept_server_voice_frame(&mut self.voice_state, &host_stream, &frame)?.is_some() {
            return Ok(Some(frame));
        }
        if !frame.binary.is_empty() {
            return Err(FrameError::Protocol(format!(
                "unexpected binary data on {}",
                envelope.kind
            )));
        }

        if envelope.stream.0.starts_with("/host/") {
            match envelope.kind {
                FrameKind::HostBootstrap => {
                    let payload: HostBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    for agent in payload.agents {
                        let stream_parts = parse_agent_stream(&agent.instance_stream);
                        assert_eq!(
                            agent.agent_id, stream_parts.agent_id,
                            "host_bootstrap agent_id {} does not match instance_stream {}",
                            agent.agent_id, agent.instance_stream
                        );
                        assert!(
                            !self.outgoing_seq.contains_key(&agent.instance_stream),
                            "duplicate bootstrapped agent stream {}",
                            agent.instance_stream
                        );
                        self.outgoing_seq.insert(agent.instance_stream, 0);
                    }
                }
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
                FrameKind::SessionSummaryCountUpdated => {
                    let _: SessionSummaryCountUpdatedPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::ProjectNotify => {
                    let _: ProjectNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CustomAgentNotify => {
                    let _: CustomAgentNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::SteeringNotify => {
                    let _: SteeringNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::SkillNotify => {
                    let _: SkillNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::McpServerNotify => {
                    let _: McpServerNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::WorkflowNotify => {
                    let _: WorkflowNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::WorkflowRunNotify => {
                    let _: WorkflowRunNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamNotify => {
                    let _: TeamNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamMemberNotify => {
                    let _: TeamMemberNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamMemberBindingNotify => {
                    let _: TeamMemberBindingNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamPresetCatalogNotify => {
                    let _: TeamPresetCatalogNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamDraftNotify => {
                    let _: TeamDraftNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamMemberShuffleSuggestionNotify => {
                    let _: protocol::TeamMemberShuffleSuggestionNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TeamContextCompactionNotify => {
                    let _: TeamContextCompactionNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::AgentsViewPreferencesNotify => {
                    let _: AgentsViewPreferencesNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::HostSettings => {
                    let _: HostSettingsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::SettingsWriteResult => {
                    let _: protocol::SettingsWriteResultPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::AgentActivitySummary => {
                    let _: AgentActivitySummaryPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::TaskTokenUsage => {
                    let _: TaskTokenUsagePayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::MobileAccessState => {
                    let _: MobileAccessStatePayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::MobilePairingOffer => {
                    let _: MobilePairingOfferPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::BackendSetup => {
                    let _: BackendSetupPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::BackendConfigSchemas => {
                    let _: BackendConfigSchemasPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::BackendConfigSnapshots => {
                    let _: BackendConfigSnapshotsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::BackendCapacity => {
                    let _: BackendCapacityPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::SessionSchemas => {
                    let _: SessionSchemasPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::LaunchProfileCatalogNotify => {
                    let _: LaunchProfileCatalogPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CommandError => {
                    let _: CommandErrorPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::AgentTurnStateNotify => {
                    let _: AgentTurnStateNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let streams_to_remove = self
                        .outgoing_seq
                        .keys()
                        .filter(|stream| stream.0.starts_with("/agent/"))
                        .filter_map(|stream| {
                            let parts = parse_agent_stream(stream);
                            (parts.agent_id == payload.agent_id).then_some(stream.clone())
                        })
                        .collect::<Vec<_>>();
                    for stream in streams_to_remove {
                        self.outgoing_seq.remove(&stream);
                    }
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
                FrameKind::AgentBootstrap => {
                    assert_eq!(
                        envelope.seq, 0,
                        "AgentBootstrap must be seq 0 on agent stream {}, got seq {}",
                        envelope.stream, envelope.seq
                    );
                    let _: AgentBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "AgentBootstrap on stream {} before HostBootstrap/NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::AgentStart => {
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
                        "AgentError on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::AgentRenamed => {
                    let payload: AgentRenamedPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "agent_renamed payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "AgentRenamed on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::AgentActivityStats => {
                    let payload: AgentActivityStatsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "agent_activity_stats payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "AgentActivityStats on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::ContextCompactionNotify => {
                    let payload: ContextCompactionNotifyPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "context_compaction_notify payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                }
                FrameKind::ContextCompactionCapability => {
                    let payload: ContextCompactionCapabilityPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "context_compaction_capability payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                }
                FrameKind::ChatEvent => {
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "ChatEvent on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::SessionHistory => {
                    let payload: SessionHistoryPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let stream_parts = parse_agent_stream(&envelope.stream);
                    assert_eq!(
                        payload.agent_id, stream_parts.agent_id,
                        "session_history payload agent_id {} does not match stream {}",
                        payload.agent_id, envelope.stream
                    );
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "SessionHistory on stream {} before NewAgent",
                        envelope.stream
                    );
                }
                FrameKind::SessionSettings => {
                    let _: SessionSettingsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::QueuedMessages => {
                    let _: QueuedMessagesPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
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
                FrameKind::ProjectBootstrap => {
                    let _: ProjectBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    self.outgoing_seq
                        .entry(envelope.stream.clone())
                        .or_insert(0);
                }
                FrameKind::ProjectFileList => {
                    let _: ProjectFileListPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::ProjectGitStatus => {
                    let _: ProjectGitStatusPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelOverview => {
                    let _: CodeIntelOverviewPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelStatus => {
                    let _: CodeIntelStatusPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelFileModel => {
                    let _: CodeIntelFileModelPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelDiagnostics => {
                    let _: CodeIntelDiagnosticsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelHoverResult => {
                    let _: CodeIntelHoverResultPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelNavigateResult => {
                    let _: CodeIntelNavigateResultPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelReferencesResults => {
                    let _: CodeIntelReferencesResultsPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelReferencesComplete => {
                    let _: CodeIntelReferencesCompletePayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CodeIntelError => {
                    let _: CodeIntelErrorPayload =
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
                FrameKind::ProjectEvent => {
                    let _: ProjectEventPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::CommandError => {
                    let _: CommandErrorPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on project stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/review/") {
            match envelope.kind {
                FrameKind::ReviewBootstrap => {
                    let _: ReviewBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    self.outgoing_seq
                        .entry(envelope.stream.clone())
                        .or_insert(0);
                }
                FrameKind::ReviewEvent => {
                    let payload: ReviewEventPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    let review_id = review_id_from_stream(&envelope.stream).unwrap_or("<unknown>");
                    match &payload {
                        ReviewEventPayload::StatusChanged { status } => {
                            tracing::debug!(
                                review_id,
                                stream = %envelope.stream,
                                seq = envelope.seq,
                                event_kind = payload.kind_name(),
                                status = status.status_label(),
                                "received review_event"
                            );
                        }
                        ReviewEventPayload::AiReviewerChanged { state } => {
                            tracing::debug!(
                                review_id,
                                stream = %envelope.stream,
                                seq = envelope.seq,
                                event_kind = payload.kind_name(),
                                status = state.status.status_label(),
                                reviewer_agent_id = state
                                    .agent_id
                                    .as_ref()
                                    .map(|id| id.0.as_str())
                                    .unwrap_or("<none>"),
                                error_len = state.error.as_ref().map_or(0, String::len),
                                "received review_event"
                            );
                        }
                        ReviewEventPayload::Error { error } => {
                            tracing::debug!(
                                review_id,
                                stream = %envelope.stream,
                                seq = envelope.seq,
                                event_kind = payload.kind_name(),
                                code = error.code.code_name(),
                                context = error.context.kind_name(),
                                fatal = error.fatal,
                                message_len = error.message.len(),
                                "received review_event"
                            );
                        }
                        _ => {
                            tracing::debug!(
                                review_id,
                                stream = %envelope.stream,
                                seq = envelope.seq,
                                event_kind = payload.kind_name(),
                                "received review_event"
                            );
                        }
                    }
                    self.outgoing_seq
                        .entry(envelope.stream.clone())
                        .or_insert(0);
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on review stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/browse/") {
            match envelope.kind {
                FrameKind::BrowseBootstrap => {
                    let _: BrowseBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::HostBrowseOpened => {
                    let _: protocol::HostBrowseOpenedPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::HostBrowseEntries => {
                    let _: protocol::HostBrowseEntriesPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                FrameKind::HostBrowseError => {
                    let _: protocol::HostBrowseErrorPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                }
                other => {
                    panic!(
                        "unexpected server frame kind {} on browse stream {}",
                        other, envelope.stream
                    );
                }
            }
        } else if envelope.stream.0.starts_with("/terminal/") {
            match envelope.kind {
                FrameKind::TerminalBootstrap => {
                    assert_eq!(
                        envelope.seq, 0,
                        "TerminalBootstrap must be seq 0 on terminal stream {}, got seq {}",
                        envelope.stream, envelope.seq
                    );
                    let _: TerminalBootstrapPayload =
                        envelope.parse_payload().map_err(FrameError::Json)?;
                    assert!(
                        self.outgoing_seq.contains_key(&envelope.stream),
                        "TerminalBootstrap on stream {} before NewTerminal",
                        envelope.stream
                    );
                }
                FrameKind::TerminalStart => {
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

        Ok(Some(frame))
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

    fn review_stream(&mut self, review_id: &ReviewId) -> StreamPath {
        let stream = StreamPath(format!("/review/{}", review_id));
        self.outgoing_seq.entry(stream.clone()).or_insert(0);
        stream
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

fn review_id_from_stream(stream: &StreamPath) -> Option<&str> {
    stream
        .0
        .strip_prefix("/review/")
        .filter(|id| !id.is_empty())
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
        voice_state: ServerVoiceState::default(),
    })
}

pub async fn connect_uds(
    path: impl AsRef<Path>,
    config: &ClientConfig,
) -> Result<Connection, HandshakeError> {
    #[cfg(unix)]
    {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|err| HandshakeError::Frame(FrameError::Io(err)))?;
        connect(config, stream).await
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = config;
        Err(HandshakeError::Frame(FrameError::Io(io::Error::new(
            ErrorKind::Unsupported,
            "Unix domain sockets are not supported on this platform",
        ))))
    }
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
    incoming_seq
        .validate(&response.stream, response.seq, response.kind)
        .map_err(|err| HandshakeError::Frame(err.into()))?;

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
                reader: FrameReader::new(Box::new(reader)),
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
