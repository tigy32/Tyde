use std::collections::{HashMap, VecDeque};
use std::fmt;

use crate::types::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentCompactNotifyPayload, AgentCompactPayload,
    BrowseBootstrapPayload, CloseAgentPayload, NewTerminalPayload, ProjectBootstrapPayload,
    ReviewBootstrapPayload, TeamCompactNotifyPayload, TeamCompactPayload,
    TeamContextCompactionNotifyPayload, TerminalBootstrapPayload,
};
use crate::{
    AgentActivityStatsPayload, AgentActivitySummaryPayload, AgentClosedPayload, AgentOrigin,
    AgentStartPayload, AgentsViewPreferencesNotifyPayload, BackendCapacityPayload,
    BackendConfigSchemasPayload, BackendConfigSnapshotsPayload, BackendKind,
    BackendNativeSettingsWritePayload, BackendSettingsRefreshPayload, BackendSetupPayload,
    CancelWorkflowPayload, ChatEvent, ClientErrorPayload, CodeIntelDiagnosticsPayload,
    CodeIntelErrorPayload, CodeIntelFileModelPayload, CodeIntelHoverResultPayload,
    CodeIntelNavigateResultPayload, CodeIntelOverviewPayload, CodeIntelReferencesCompletePayload,
    CodeIntelReferencesResultsPayload, CodeIntelStatusPayload, CommandErrorPayload,
    ContextCompactionCapabilityPayload, ContextCompactionNotifyPayload, CustomAgentDeletePayload,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, DeleteSessionPayload, Envelope,
    FetchSessionHistoryPayload, FrameKind, HeartbeatPayload, HostBootstrapPayload,
    HostBrowseClosePayload, HostBrowseEntriesPayload, HostBrowseErrorPayload,
    HostBrowseListPayload, HostBrowseOpenedPayload, HostBrowseStartPayload, HostSettingsPayload,
    InvokeSettingsActionPayload, LaunchProfileCatalogPayload, ListSessionsPayload,
    LoadAgentPayload, McpServerDeletePayload, McpServerNotifyPayload, McpServerUpsertPayload,
    MobileAccessStatePayload, MobileDeviceRenamePayload, MobileDeviceRevokePayload,
    MobilePairingCancelPayload, MobilePairingOfferPayload, MobilePairingStartPayload,
    MobilePushSubscribePayload, MobilePushUnsubscribePayload, NewAgentPayload,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload,
    ProjectEventPayload, ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectNotifyPayload, ProjectRenamePayload, ProjectReorderPayload,
    ProjectSearchCompletePayload, ProjectSearchResultsPayload, ReviewEventPayload,
    RunBackendSetupPayload, SETTINGS_WRITE_MAX_OPS, SessionHistoryPayload, SessionListPayload,
    SessionSchemasPayload, SessionSummaryCountUpdatedPayload, SetAgentGroupsPayload,
    SetAgentPinsPayload, SetAgentTagsPayload, SetAgentsSmartViewsPayload,
    SetAgentsViewPreferencesPayload, SettingsWritePayload, SettingsWriteResultPayload,
    SkillNotifyPayload, SkillRefreshPayload, SpawnAgentPayload, SteeringDeletePayload,
    SteeringNotifyPayload, SteeringUpsertPayload, StreamPath, TaskTokenUsagePayload,
    TeamCreatePayload, TeamDeletePayload, TeamDraftApplyTemplatePayload, TeamDraftCommitPayload,
    TeamDraftCreatePayload, TeamDraftDiscardPayload, TeamDraftNotifyPayload,
    TeamDraftShufflePayload, TeamDraftUpdatePayload, TeamMemberActivatePayload,
    TeamMemberBindingNotifyPayload, TeamMemberCreatePayload, TeamMemberDeletePayload,
    TeamMemberNotifyPayload, TeamMemberShufflePayload, TeamMemberShuffleSuggestionNotifyPayload,
    TeamMemberUpdatePayload, TeamNotifyPayload, TeamPresetCatalogNotifyPayload, TeamRenamePayload,
    TeamSetManagerPayload, TerminalCreatePayload, TerminalErrorPayload, TerminalExitPayload,
    TerminalOutputPayload, TriggerWorkflowPayload, WelcomePayload, WorkbenchCreatePayload,
    WorkbenchRemovePayload, WorkflowNotifyPayload, WorkflowRefreshPayload,
    WorkflowRunNotifyPayload, parse_json_pointer,
};

const DEFAULT_HISTORY_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub struct ProtocolValidator {
    history_limit: usize,
    recent: VecDeque<ObservedFrame>,
    host_streams: HashMap<StreamPath, HostStreamState>,
    agent_streams: HashMap<StreamPath, AgentStreamState>,
    project_streams: HashMap<StreamPath, BootstrapStreamState>,
    review_streams: HashMap<StreamPath, BootstrapStreamState>,
    browse_streams: HashMap<StreamPath, BootstrapStreamState>,
    terminal_streams: HashMap<StreamPath, BootstrapStreamState>,
    voice_streams: HashMap<StreamPath, VoiceStreamState>,
}

#[derive(Debug, Clone, Default)]
struct VoiceStreamState {
    generation: u64,
    session_id: Option<crate::VoiceSessionId>,
    last_input_media_seq: Option<u64>,
    last_output_media_seq: Option<u64>,
    stopped: bool,
}

fn validate_voice_identity(
    validator: &ProtocolValidator,
    envelope: &Envelope,
    state: &VoiceStreamState,
    id: &crate::VoiceSessionId,
    generation: u64,
) -> Result<(), ProtocolViolation> {
    if state.stopped || state.generation != generation || state.session_id.as_ref() != Some(id) {
        Err(validator.violation(envelope, None, "stale or foreign voice session".into()))
    } else {
        Ok(())
    }
}

impl Default for ProtocolValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolValidator {
    pub fn new() -> Self {
        Self {
            history_limit: DEFAULT_HISTORY_LIMIT,
            recent: VecDeque::with_capacity(DEFAULT_HISTORY_LIMIT),
            host_streams: HashMap::new(),
            agent_streams: HashMap::new(),
            project_streams: HashMap::new(),
            review_streams: HashMap::new(),
            browse_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
            voice_streams: HashMap::new(),
        }
    }

    pub fn with_history_limit(history_limit: usize) -> Self {
        Self {
            history_limit: history_limit.max(1),
            recent: VecDeque::with_capacity(history_limit.max(1)),
            host_streams: HashMap::new(),
            agent_streams: HashMap::new(),
            project_streams: HashMap::new(),
            review_streams: HashMap::new(),
            browse_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
            voice_streams: HashMap::new(),
        }
    }

    pub fn validate_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        self.record(envelope);

        if envelope.stream.0.starts_with("/host/") {
            return self.validate_host_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/agent/") {
            return self.validate_agent_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/project/") {
            return self.validate_project_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/review/") {
            return self.validate_review_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/browse/") {
            return self.validate_browse_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/terminal/") {
            return self.validate_terminal_envelope(envelope);
        }
        if envelope.stream.0.starts_with("/voice") {
            return self.validate_voice_envelope(envelope);
        }

        Ok(())
    }

    fn validate_voice_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        let current = self
            .voice_streams
            .get(&envelope.stream)
            .cloned()
            .unwrap_or_default();
        match envelope.kind {
            FrameKind::VoiceCapabilities => {
                if envelope.stream.0 != "/voice" {
                    return Err(self.violation(
                        envelope,
                        None,
                        "VoiceCapabilities must use /voice".into(),
                    ));
                }
                let payload: crate::VoiceCapabilitiesPayload =
                    envelope.parse_payload().map_err(|e| {
                        self.violation(envelope, None, format!("invalid VoiceCapabilities: {e}"))
                    })?;
                if !payload.valid() {
                    return Err(self.violation(
                        envelope,
                        None,
                        "invalid VoiceCapabilities values".into(),
                    ));
                }
                Ok(())
            }
            FrameKind::VoiceStart => {
                if envelope.stream.0 != "/voice" {
                    return Err(self.violation(
                        envelope,
                        None,
                        "VoiceStart must use /voice".into(),
                    ));
                }
                let payload: crate::VoiceStartPayload = envelope.parse_payload().map_err(|e| {
                    self.violation(envelope, None, format!("invalid VoiceStart: {e}"))
                })?;
                if payload.generation <= current.generation {
                    return Err(self.violation(
                        envelope,
                        None,
                        "voice generation did not advance".into(),
                    ));
                }
                self.voice_streams.insert(
                    envelope.stream.clone(),
                    VoiceStreamState {
                        generation: payload.generation,
                        session_id: None,
                        last_input_media_seq: None,
                        last_output_media_seq: None,
                        stopped: false,
                    },
                );
                Ok(())
            }
            FrameKind::VoiceAccepted => {
                if envelope.stream.0 != "/voice" {
                    return Err(self.violation(
                        envelope,
                        None,
                        "VoiceAccepted must use /voice".into(),
                    ));
                }
                let payload: crate::VoiceAcceptedPayload =
                    envelope.parse_payload().map_err(|e| {
                        self.violation(envelope, None, format!("invalid VoiceAccepted: {e}"))
                    })?;
                if !payload.uplink.valid() || !payload.downlink.valid() {
                    return Err(self.violation(
                        envelope,
                        None,
                        "invalid accepted voice format".into(),
                    ));
                }
                let state = VoiceStreamState {
                    generation: payload.generation,
                    session_id: Some(payload.session_id.clone()),
                    last_input_media_seq: None,
                    last_output_media_seq: None,
                    stopped: false,
                };
                self.voice_streams
                    .insert(envelope.stream.clone(), state.clone());
                self.voice_streams.insert(
                    StreamPath(format!("/voice/{}", payload.session_id.0)),
                    state,
                );
                Ok(())
            }
            FrameKind::VoiceAudio => {
                let payload: crate::VoiceAudioPayload = envelope.parse_payload().map_err(|e| {
                    self.violation(envelope, None, format!("invalid VoiceAudio: {e}"))
                })?;
                validate_voice_identity(
                    self,
                    envelope,
                    &current,
                    &payload.session_id,
                    payload.generation,
                )?;
                let last = match payload.direction {
                    crate::VoiceDirection::Input => current.last_input_media_seq,
                    crate::VoiceDirection::Output => current.last_output_media_seq,
                };
                if last.is_some_and(|last| payload.first_media_seq < last) {
                    return Err(self.violation(
                        envelope,
                        None,
                        "voice media sequence moved backwards".into(),
                    ));
                }
                let mut next = current;
                let end = payload.first_media_seq + payload.packet_lengths.len() as u64;
                match payload.direction {
                    crate::VoiceDirection::Input => next.last_input_media_seq = Some(end),
                    crate::VoiceDirection::Output => next.last_output_media_seq = Some(end),
                };
                self.voice_streams.insert(envelope.stream.clone(), next);
                Ok(())
            }
            FrameKind::VoiceInputEnd | FrameKind::VoiceInterrupt | FrameKind::VoiceOutput => {
                let payload: crate::VoiceSessionPayload =
                    envelope.parse_payload().map_err(|e| {
                        self.violation(envelope, None, format!("invalid voice control: {e}"))
                    })?;
                validate_voice_identity(
                    self,
                    envelope,
                    &current,
                    &payload.session_id,
                    payload.generation,
                )
            }
            FrameKind::VoiceTranscript => {
                let payload: crate::VoiceTranscriptPayload =
                    envelope.parse_payload().map_err(|e| {
                        self.violation(envelope, None, format!("invalid VoiceTranscript: {e}"))
                    })?;
                validate_voice_identity(
                    self,
                    envelope,
                    &current,
                    &payload.session_id,
                    payload.generation,
                )
            }
            FrameKind::VoiceState => {
                let payload: crate::VoiceStatePayload = envelope.parse_payload().map_err(|e| {
                    self.violation(envelope, None, format!("invalid VoiceState: {e}"))
                })?;
                validate_voice_identity(
                    self,
                    envelope,
                    &current,
                    &payload.session_id,
                    payload.generation,
                )
            }
            FrameKind::VoiceStop => {
                let payload: crate::VoiceStopPayload = envelope.parse_payload().map_err(|e| {
                    self.violation(envelope, None, format!("invalid VoiceStop: {e}"))
                })?;
                validate_voice_identity(
                    self,
                    envelope,
                    &current,
                    &payload.session_id,
                    payload.generation,
                )?;
                let mut next = current;
                next.stopped = true;
                self.voice_streams.insert(envelope.stream.clone(), next);
                Ok(())
            }
            FrameKind::VoiceError => {
                let payload: crate::VoiceErrorPayload = envelope.parse_payload().map_err(|e| {
                    self.violation(envelope, None, format!("invalid VoiceError: {e}"))
                })?;
                if let Some(id) = &payload.session_id {
                    validate_voice_identity(self, envelope, &current, id, payload.generation)
                } else if envelope.stream.0 == "/voice" {
                    Ok(())
                } else {
                    Err(self.violation(
                        envelope,
                        None,
                        "pre-session VoiceError must use /voice".into(),
                    ))
                }
            }
            other => Err(self.violation(
                envelope,
                None,
                format!("unexpected {other} on voice stream"),
            )),
        }
    }

    /// Applies the backend-native `ConversationCleared` boundary for an agent
    /// stream. That wire notification is not a client protocol frame, so the
    /// owning backend bridge must call this explicit typed reset point.
    pub fn conversation_cleared(&mut self, stream: &StreamPath) {
        if let Some(state) = self.agent_streams.get_mut(stream) {
            state.compaction_operations.clear();
        }
    }

    fn validate_host_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        let host_state = self
            .host_streams
            .get(&envelope.stream)
            .copied()
            .unwrap_or_default();

        match envelope.kind {
            FrameKind::Welcome => {
                if envelope.seq != 0 {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!("Welcome must be seq 0 on host stream {}", envelope.stream),
                    ));
                }
                let _: WelcomePayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse Welcome payload: {error}"),
                    )
                })?;
                self.host_streams.insert(
                    envelope.stream.clone(),
                    HostStreamState {
                        saw_welcome: true,
                        saw_bootstrap: host_state.saw_bootstrap,
                    },
                );
                Ok(())
            }
            FrameKind::HostBootstrap => {
                if host_state.saw_bootstrap {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!("duplicate HostBootstrap for stream {}", envelope.stream),
                    ));
                }
                if host_state.saw_welcome && envelope.seq != 1 {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!(
                            "HostBootstrap must be seq 1 after Welcome on host stream {}, got {}",
                            envelope.stream, envelope.seq
                        ),
                    ));
                }
                if !host_state.saw_welcome && !matches!(envelope.seq, 0 | 1) {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!(
                            "HostBootstrap must be first observed host event with seq 0 or 1 on {}, got seq {}",
                            envelope.stream, envelope.seq
                        ),
                    ));
                }
                let payload: HostBootstrapPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse HostBootstrap payload: {error}"),
                    )
                })?;
                validate_session_list_page(self, envelope, "HostBootstrap", &payload.session_list)?;
                for agent in payload.agents {
                    self.register_agent_stream_from_new_agent(envelope, agent)?;
                }
                self.host_streams.insert(
                    envelope.stream.clone(),
                    HostStreamState {
                        saw_welcome: host_state.saw_welcome,
                        saw_bootstrap: true,
                    },
                );
                Ok(())
            }
            FrameKind::Reject => Ok(()),
            _ if !host_state.saw_bootstrap => Err(self.violation(
                envelope,
                None,
                format!(
                    "received host frame {} before HostBootstrap on {}",
                    envelope.kind, envelope.stream
                ),
            )),
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse NewAgent payload: {error}"),
                    )
                })?;
                self.register_agent_stream_from_new_agent(envelope, payload)
            }
            FrameKind::AgentClosed => {
                let payload: AgentClosedPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse AgentClosed payload: {error}"),
                    )
                })?;

                let streams_to_remove = self
                    .agent_streams
                    .iter()
                    .filter_map(|(stream, state)| {
                        (state.agent_id == payload.agent_id).then_some(stream.clone())
                    })
                    .collect::<Vec<_>>();
                let removed = streams_to_remove.len();
                for stream in streams_to_remove {
                    self.agent_streams.remove(&stream);
                }
                if removed == 0 {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!(
                            "AgentClosed referenced unknown agent_id {}",
                            payload.agent_id
                        ),
                    ));
                }
                Ok(())
            }
            FrameKind::HostSettings => {
                parse_host_payload::<HostSettingsPayload>(self, envelope, "HostSettings")
            }
            FrameKind::VoiceCapabilities => {
                let payload: crate::VoiceCapabilitiesPayload =
                    envelope.parse_payload().map_err(|error| {
                        self.violation(
                            envelope,
                            None,
                            format!("failed to parse VoiceCapabilities payload: {error}"),
                        )
                    })?;
                if payload.valid() {
                    Ok(())
                } else {
                    Err(self.violation(envelope, None, "invalid VoiceCapabilities values".into()))
                }
            }
            FrameKind::AgentActivitySummary => parse_host_payload::<AgentActivitySummaryPayload>(
                self,
                envelope,
                "AgentActivitySummary",
            ),
            FrameKind::AgentActivityStats => Err(self.violation(
                envelope,
                None,
                format!(
                    "AgentActivityStats is an agent-stream-only frame, received on host stream {}",
                    envelope.stream
                ),
            )),
            FrameKind::TaskTokenUsage => {
                parse_host_payload::<TaskTokenUsagePayload>(self, envelope, "TaskTokenUsage")
            }
            FrameKind::AgentsViewPreferencesNotify => {
                parse_host_payload::<AgentsViewPreferencesNotifyPayload>(
                    self,
                    envelope,
                    "AgentsViewPreferencesNotify",
                )
            }
            FrameKind::MobileAccessState => {
                parse_host_payload::<MobileAccessStatePayload>(self, envelope, "MobileAccessState")
            }
            FrameKind::MobilePairingOffer => parse_host_payload::<MobilePairingOfferPayload>(
                self,
                envelope,
                "MobilePairingOffer",
            ),
            FrameKind::BackendSetup => {
                parse_host_payload::<BackendSetupPayload>(self, envelope, "BackendSetup")
            }
            FrameKind::BackendConfigSchemas => parse_host_payload::<BackendConfigSchemasPayload>(
                self,
                envelope,
                "BackendConfigSchemas",
            ),
            FrameKind::BackendConfigSnapshots => {
                parse_host_payload::<BackendConfigSnapshotsPayload>(
                    self,
                    envelope,
                    "BackendConfigSnapshots",
                )
            }
            FrameKind::BackendCapacity => {
                parse_host_payload::<BackendCapacityPayload>(self, envelope, "BackendCapacity")
            }
            FrameKind::SessionSchemas => {
                parse_host_payload::<SessionSchemasPayload>(self, envelope, "SessionSchemas")
            }
            FrameKind::LaunchProfileCatalogNotify => parse_host_payload::<
                LaunchProfileCatalogPayload,
            >(
                self, envelope, "LaunchProfileCatalogNotify"
            ),
            FrameKind::SessionList => {
                let payload: SessionListPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse SessionList payload: {error}"),
                    )
                })?;
                validate_session_list_page(self, envelope, "SessionList", &payload.page)
            }
            FrameKind::SessionSummaryCountUpdated => parse_host_payload::<
                SessionSummaryCountUpdatedPayload,
            >(
                self, envelope, "SessionSummaryCountUpdated"
            ),
            FrameKind::CommandError => {
                parse_host_payload::<CommandErrorPayload>(self, envelope, "CommandError")
            }
            FrameKind::ProjectNotify => {
                parse_host_payload::<ProjectNotifyPayload>(self, envelope, "ProjectNotify")
            }
            FrameKind::WorkflowNotify => {
                parse_host_payload::<WorkflowNotifyPayload>(self, envelope, "WorkflowNotify")
            }
            FrameKind::WorkflowRunNotify => {
                parse_host_payload::<WorkflowRunNotifyPayload>(self, envelope, "WorkflowRunNotify")
            }
            FrameKind::CustomAgentNotify => {
                parse_host_payload::<CustomAgentNotifyPayload>(self, envelope, "CustomAgentNotify")
            }
            FrameKind::SteeringNotify => {
                parse_host_payload::<SteeringNotifyPayload>(self, envelope, "SteeringNotify")
            }
            FrameKind::SkillNotify => {
                parse_host_payload::<SkillNotifyPayload>(self, envelope, "SkillNotify")
            }
            FrameKind::McpServerNotify => {
                parse_host_payload::<McpServerNotifyPayload>(self, envelope, "McpServerNotify")
            }
            FrameKind::TeamNotify => {
                parse_host_payload::<TeamNotifyPayload>(self, envelope, "TeamNotify")
            }
            FrameKind::TeamMemberNotify => {
                parse_host_payload::<TeamMemberNotifyPayload>(self, envelope, "TeamMemberNotify")
            }
            FrameKind::TeamMemberBindingNotify => parse_host_payload::<
                TeamMemberBindingNotifyPayload,
            >(
                self, envelope, "TeamMemberBindingNotify"
            ),
            FrameKind::TeamPresetCatalogNotify => parse_host_payload::<
                TeamPresetCatalogNotifyPayload,
            >(
                self, envelope, "TeamPresetCatalogNotify"
            ),
            FrameKind::TeamDraftNotify => {
                parse_host_payload::<TeamDraftNotifyPayload>(self, envelope, "TeamDraftNotify")
            }
            FrameKind::TeamMemberShuffleSuggestionNotify => {
                parse_host_payload::<TeamMemberShuffleSuggestionNotifyPayload>(
                    self,
                    envelope,
                    "TeamMemberShuffleSuggestionNotify",
                )
            }
            FrameKind::SettingsWrite => {
                let payload: SettingsWritePayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse SettingsWrite payload: {error}"),
                    )
                })?;
                validate_settings_write_payload(&payload)
                    .map_err(|message| self.violation(envelope, None, message))
            }
            FrameKind::BackendNativeSettingsWrite => parse_host_payload::<
                BackendNativeSettingsWritePayload,
            >(
                self, envelope, "BackendNativeSettingsWrite"
            ),
            FrameKind::InvokeSettingsAction => parse_host_payload::<InvokeSettingsActionPayload>(
                self,
                envelope,
                "InvokeSettingsAction",
            ),
            FrameKind::SettingsWriteResult => parse_host_payload::<SettingsWriteResultPayload>(
                self,
                envelope,
                "SettingsWriteResult",
            ),
            FrameKind::SetAgentsViewPreferences => parse_host_payload::<
                SetAgentsViewPreferencesPayload,
            >(
                self, envelope, "SetAgentsViewPreferences"
            ),
            FrameKind::SetAgentsSmartViews => parse_host_payload::<SetAgentsSmartViewsPayload>(
                self,
                envelope,
                "SetAgentsSmartViews",
            ),
            FrameKind::SetAgentTags => {
                parse_host_payload::<SetAgentTagsPayload>(self, envelope, "SetAgentTags")
            }
            FrameKind::SetAgentPins => {
                parse_host_payload::<SetAgentPinsPayload>(self, envelope, "SetAgentPins")
            }
            FrameKind::SetAgentGroups => {
                parse_host_payload::<SetAgentGroupsPayload>(self, envelope, "SetAgentGroups")
            }
            FrameKind::MobilePairingStart => parse_host_payload::<MobilePairingStartPayload>(
                self,
                envelope,
                "MobilePairingStart",
            ),
            FrameKind::MobilePairingCancel => parse_host_payload::<MobilePairingCancelPayload>(
                self,
                envelope,
                "MobilePairingCancel",
            ),
            FrameKind::MobileDeviceRevoke => parse_host_payload::<MobileDeviceRevokePayload>(
                self,
                envelope,
                "MobileDeviceRevoke",
            ),
            FrameKind::MobileDeviceRename => parse_host_payload::<MobileDeviceRenamePayload>(
                self,
                envelope,
                "MobileDeviceRename",
            ),
            FrameKind::MobilePushSubscribe => parse_host_payload::<MobilePushSubscribePayload>(
                self,
                envelope,
                "MobilePushSubscribe",
            ),
            FrameKind::MobilePushUnsubscribe => parse_host_payload::<MobilePushUnsubscribePayload>(
                self,
                envelope,
                "MobilePushUnsubscribe",
            ),
            FrameKind::ClientError => {
                parse_host_payload::<ClientErrorPayload>(self, envelope, "ClientError")
            }
            FrameKind::Heartbeat => {
                parse_host_payload::<HeartbeatPayload>(self, envelope, "Heartbeat")
            }
            FrameKind::HeartbeatAck => {
                parse_host_payload::<HeartbeatPayload>(self, envelope, "HeartbeatAck")
            }
            FrameKind::TriggerWorkflow => {
                parse_host_payload::<TriggerWorkflowPayload>(self, envelope, "TriggerWorkflow")
            }
            FrameKind::CancelWorkflow => {
                parse_host_payload::<CancelWorkflowPayload>(self, envelope, "CancelWorkflow")
            }
            FrameKind::WorkflowRefresh => {
                parse_host_payload::<WorkflowRefreshPayload>(self, envelope, "WorkflowRefresh")
            }
            FrameKind::SpawnAgent => {
                let payload: SpawnAgentPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse SpawnAgent payload: {error}"),
                    )
                })?;
                validate_spawn_agent_payload(&payload)
                    .map_err(|message| self.violation(envelope, None, message))
            }
            FrameKind::ListSessions => {
                parse_host_payload::<ListSessionsPayload>(self, envelope, "ListSessions")
            }
            FrameKind::DeleteSession => {
                parse_host_payload::<DeleteSessionPayload>(self, envelope, "DeleteSession")
            }
            FrameKind::ProjectCreate => {
                parse_host_payload::<ProjectCreatePayload>(self, envelope, "ProjectCreate")
            }
            FrameKind::ProjectRename => {
                parse_host_payload::<ProjectRenamePayload>(self, envelope, "ProjectRename")
            }
            FrameKind::ProjectReorder => {
                parse_host_payload::<ProjectReorderPayload>(self, envelope, "ProjectReorder")
            }
            FrameKind::ProjectAddRoot => {
                parse_host_payload::<ProjectAddRootPayload>(self, envelope, "ProjectAddRoot")
            }
            FrameKind::ProjectDeleteRoot => {
                parse_host_payload::<ProjectDeleteRootPayload>(self, envelope, "ProjectDeleteRoot")
            }
            FrameKind::ProjectDelete => {
                parse_host_payload::<ProjectDeletePayload>(self, envelope, "ProjectDelete")
            }
            FrameKind::WorkbenchCreate => {
                parse_host_payload::<WorkbenchCreatePayload>(self, envelope, "WorkbenchCreate")
            }
            FrameKind::WorkbenchRemove => {
                parse_host_payload::<WorkbenchRemovePayload>(self, envelope, "WorkbenchRemove")
            }
            FrameKind::CustomAgentUpsert => {
                parse_host_payload::<CustomAgentUpsertPayload>(self, envelope, "CustomAgentUpsert")
            }
            FrameKind::CustomAgentDelete => {
                parse_host_payload::<CustomAgentDeletePayload>(self, envelope, "CustomAgentDelete")
            }
            FrameKind::SteeringUpsert => {
                parse_host_payload::<SteeringUpsertPayload>(self, envelope, "SteeringUpsert")
            }
            FrameKind::SteeringDelete => {
                parse_host_payload::<SteeringDeletePayload>(self, envelope, "SteeringDelete")
            }
            FrameKind::SkillRefresh => {
                parse_host_payload::<SkillRefreshPayload>(self, envelope, "SkillRefresh")
            }
            FrameKind::BackendSettingsRefresh => {
                parse_host_payload::<BackendSettingsRefreshPayload>(
                    self,
                    envelope,
                    "BackendSettingsRefresh",
                )
            }
            FrameKind::McpServerUpsert => {
                parse_host_payload::<McpServerUpsertPayload>(self, envelope, "McpServerUpsert")
            }
            FrameKind::McpServerDelete => {
                parse_host_payload::<McpServerDeletePayload>(self, envelope, "McpServerDelete")
            }
            FrameKind::TeamCreate => {
                parse_host_payload::<TeamCreatePayload>(self, envelope, "TeamCreate")
            }
            FrameKind::TeamRename => {
                parse_host_payload::<TeamRenamePayload>(self, envelope, "TeamRename")
            }
            FrameKind::TeamDelete => {
                parse_host_payload::<TeamDeletePayload>(self, envelope, "TeamDelete")
            }
            FrameKind::TeamSetManager => {
                parse_host_payload::<TeamSetManagerPayload>(self, envelope, "TeamSetManager")
            }
            FrameKind::TeamMemberCreate => {
                parse_host_payload::<TeamMemberCreatePayload>(self, envelope, "TeamMemberCreate")
            }
            FrameKind::TeamMemberUpdate => {
                parse_host_payload::<TeamMemberUpdatePayload>(self, envelope, "TeamMemberUpdate")
            }
            FrameKind::TeamMemberDelete => {
                parse_host_payload::<TeamMemberDeletePayload>(self, envelope, "TeamMemberDelete")
            }
            FrameKind::TeamMemberActivate => parse_host_payload::<TeamMemberActivatePayload>(
                self,
                envelope,
                "TeamMemberActivate",
            ),
            FrameKind::TeamCompact => {
                parse_host_payload::<TeamCompactPayload>(self, envelope, "TeamCompact")
            }
            FrameKind::TeamCompactNotify => {
                parse_host_payload::<TeamCompactNotifyPayload>(self, envelope, "TeamCompactNotify")
            }
            FrameKind::TeamContextCompactionNotify => {
                let payload: TeamContextCompactionNotifyPayload =
                    envelope.parse_payload().map_err(|error| {
                        self.violation(
                            envelope,
                            None,
                            format!("failed to parse TeamContextCompactionNotify payload: {error}"),
                        )
                    })?;
                if let Some(member) = payload
                    .members
                    .iter()
                    .find(|member| member.logical_session_id.0.trim().is_empty())
                {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!(
                            "TeamContextCompactionNotify member {} has an empty logical_session_id",
                            member.agent_id
                        ),
                    ));
                }
                Ok(())
            }
            FrameKind::TeamDraftCreate => {
                parse_host_payload::<TeamDraftCreatePayload>(self, envelope, "TeamDraftCreate")
            }
            FrameKind::TeamDraftUpdate => {
                parse_host_payload::<TeamDraftUpdatePayload>(self, envelope, "TeamDraftUpdate")
            }
            FrameKind::TeamDraftShuffle => {
                parse_host_payload::<TeamDraftShufflePayload>(self, envelope, "TeamDraftShuffle")
            }
            FrameKind::TeamMemberShuffle => {
                parse_host_payload::<TeamMemberShufflePayload>(self, envelope, "TeamMemberShuffle")
            }
            FrameKind::TeamDraftApplyTemplate => {
                parse_host_payload::<TeamDraftApplyTemplatePayload>(
                    self,
                    envelope,
                    "TeamDraftApplyTemplate",
                )
            }
            FrameKind::TeamDraftCommit => {
                parse_host_payload::<TeamDraftCommitPayload>(self, envelope, "TeamDraftCommit")
            }
            FrameKind::TeamDraftDiscard => {
                parse_host_payload::<TeamDraftDiscardPayload>(self, envelope, "TeamDraftDiscard")
            }
            FrameKind::HostBrowseStart => {
                parse_host_payload::<HostBrowseStartPayload>(self, envelope, "HostBrowseStart")
            }
            FrameKind::HostBrowseList => {
                parse_host_payload::<HostBrowseListPayload>(self, envelope, "HostBrowseList")
            }
            FrameKind::HostBrowseClose => {
                parse_host_payload::<HostBrowseClosePayload>(self, envelope, "HostBrowseClose")
            }
            FrameKind::TerminalCreate => {
                parse_host_payload::<TerminalCreatePayload>(self, envelope, "TerminalCreate")
            }
            FrameKind::RunBackendSetup => {
                parse_host_payload::<RunBackendSetupPayload>(self, envelope, "RunBackendSetup")
            }
            FrameKind::NewTerminal => {
                parse_host_payload::<NewTerminalPayload>(self, envelope, "NewTerminal")
            }
            _ => Ok(()),
        }
    }

    fn validate_agent_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        let recent_frames: Vec<_> = self.recent.iter().cloned().collect();
        let Some(state) = self.agent_streams.get_mut(&envelope.stream) else {
            return Err(build_violation(
                &recent_frames,
                envelope,
                None,
                format!(
                    "received agent frame {} before NewAgent registered stream {}",
                    envelope.kind, envelope.stream
                ),
            ));
        };

        match envelope.kind {
            FrameKind::LoadAgent => {
                let _: LoadAgentPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse LoadAgent payload: {error}"),
                    )
                })?;
            }
            FrameKind::AgentBootstrap => {
                if state.saw_bootstrap {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("duplicate AgentBootstrap for stream {}", envelope.stream),
                    ));
                }
                if envelope.seq != 0 {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "AgentBootstrap must be seq 0 on {}, got {}",
                            envelope.stream, envelope.seq
                        ),
                    ));
                }
                let payload: AgentBootstrapPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse AgentBootstrap payload: {error}"),
                    )
                })?;
                state.saw_bootstrap = true;
                let bootstrap_events = payload.events;
                for event in bootstrap_events.iter().cloned() {
                    if let Err(violation) =
                        validate_agent_bootstrap_event(&recent_frames, envelope, state, event)
                    {
                        eprintln!(
                            "TYDE BOOTSTRAP VALIDATION FAILURE stream={} events={bootstrap_events:#?}",
                            envelope.stream
                        );
                        return Err(violation);
                    }
                }
            }
            _ if !state.saw_bootstrap => {
                return Err(build_violation(
                    &recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "received agent frame {} before AgentBootstrap on {}",
                        envelope.kind, envelope.stream
                    ),
                ));
            }
            FrameKind::AgentStart => {
                if state.saw_agent_start {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("duplicate AgentStart for stream {}", envelope.stream),
                    ));
                }
                let payload: AgentStartPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse AgentStart payload: {error}"),
                    )
                })?;
                if let Err(message) = validate_agent_origin(
                    payload.origin,
                    payload.parent_agent_id.as_ref(),
                    payload.team_id.as_ref(),
                    payload.team_member_id.as_ref(),
                    payload.workflow.as_ref(),
                ) {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        message,
                    ));
                }
                state.saw_agent_start = true;
                if payload.session_id.is_some() {
                    state.logical_session_id = payload.session_id;
                }
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse ChatEvent payload: {error}"),
                    )
                })?;
                validate_chat_event(&recent_frames, envelope, state, &event)?;
            }
            FrameKind::FetchSessionHistory => {
                let payload: FetchSessionHistoryPayload =
                    envelope.parse_payload().map_err(|error| {
                        build_violation(
                            &recent_frames,
                            envelope,
                            Some(state.backend_kind),
                            format!("failed to parse FetchSessionHistory payload: {error}"),
                        )
                    })?;
                if payload.agent_id != state.agent_id {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "FetchSessionHistory agent_id {} does not match stream agent_id {}",
                            payload.agent_id, state.agent_id
                        ),
                    ));
                }
            }
            FrameKind::SessionHistory => {
                let payload: SessionHistoryPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse SessionHistory payload: {error}"),
                    )
                })?;
                if payload.agent_id != state.agent_id {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "SessionHistory agent_id {} does not match stream agent_id {}",
                            payload.agent_id, state.agent_id
                        ),
                    ));
                }
            }
            FrameKind::AgentRenamed => {
                if !state.saw_agent_start {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "AgentRenamed arrived before AgentStart on {}",
                            envelope.stream
                        ),
                    ));
                }
            }
            FrameKind::AgentCompactNotify => {
                let _: AgentCompactNotifyPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse AgentCompactNotify payload: {error}"),
                    )
                })?;
            }
            FrameKind::ContextCompactionNotify => {
                let payload: ContextCompactionNotifyPayload =
                    envelope.parse_payload().map_err(|error| {
                        build_violation(
                            &recent_frames,
                            envelope,
                            Some(state.backend_kind),
                            format!("failed to parse ContextCompactionNotify payload: {error}"),
                        )
                    })?;
                validate_context_compaction_notify(&recent_frames, envelope, state, &payload)?;
            }
            FrameKind::ContextCompactionCapability => {
                let payload: ContextCompactionCapabilityPayload =
                    envelope.parse_payload().map_err(|error| {
                        build_violation(
                            &recent_frames,
                            envelope,
                            Some(state.backend_kind),
                            format!("failed to parse ContextCompactionCapability payload: {error}"),
                        )
                    })?;
                if payload.agent_id != state.agent_id {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "ContextCompactionCapability agent_id {} does not match stream agent_id {}",
                            payload.agent_id, state.agent_id
                        ),
                    ));
                }
            }
            FrameKind::AgentActivityStats => {
                if !state.saw_agent_start {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "AgentActivityStats arrived before AgentStart on {}",
                            envelope.stream
                        ),
                    ));
                }
                let payload: AgentActivityStatsPayload =
                    envelope.parse_payload().map_err(|error| {
                        build_violation(
                            &recent_frames,
                            envelope,
                            Some(state.backend_kind),
                            format!("failed to parse AgentActivityStats payload: {error}"),
                        )
                    })?;
                if payload.agent_id != state.agent_id {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "AgentActivityStats agent_id {} does not match stream agent_id {}",
                            payload.agent_id, state.agent_id
                        ),
                    ));
                }
            }
            FrameKind::AgentError => {}
            FrameKind::SessionSettings => {}
            FrameKind::SetSessionSettings => {}
            FrameKind::AgentCompact => {
                let _: AgentCompactPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse AgentCompact payload: {error}"),
                    )
                })?;
            }
            FrameKind::QueuedMessages => {}
            FrameKind::EditQueuedMessage => {}
            FrameKind::CancelQueuedMessage => {}
            FrameKind::CancelBackgroundTask => {}
            FrameKind::SendQueuedMessageNow => {}
            FrameKind::CloseAgent => {
                let _: CloseAgentPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse CloseAgent payload: {error}"),
                    )
                })?;
            }
            other => {
                return Err(build_violation(
                    &recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "unexpected frame kind {other} on agent stream {}",
                        envelope.stream
                    ),
                ));
            }
        }

        Ok(())
    }

    fn register_agent_stream_from_new_agent(
        &mut self,
        envelope: &Envelope,
        payload: NewAgentPayload,
    ) -> Result<(), ProtocolViolation> {
        if self.agent_streams.contains_key(&payload.instance_stream) {
            return Err(self.violation(
                envelope,
                Some(payload.backend_kind),
                format!("duplicate agent stream {}", payload.instance_stream),
            ));
        }

        validate_agent_origin(
            payload.origin,
            payload.parent_agent_id.as_ref(),
            payload.team_id.as_ref(),
            payload.team_member_id.as_ref(),
            payload.workflow.as_ref(),
        )
        .map_err(|message| self.violation(envelope, Some(payload.backend_kind), message))?;

        self.agent_streams.insert(
            payload.instance_stream,
            AgentStreamState {
                agent_id: payload.agent_id,
                backend_kind: payload.backend_kind,
                logical_session_id: payload.session_id,
                saw_bootstrap: false,
                saw_agent_start: false,
                compaction_operations: HashMap::new(),
            },
        );
        Ok(())
    }

    fn validate_project_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        validate_bootstrap_stream(
            &mut self.project_streams,
            &self.recent,
            envelope,
            FrameKind::ProjectBootstrap,
            "ProjectBootstrap",
        )?;
        match envelope.kind {
            FrameKind::ProjectBootstrap => parse_stream_payload::<ProjectBootstrapPayload>(
                &self.recent,
                envelope,
                "ProjectBootstrap",
            ),
            FrameKind::ProjectFileList => parse_stream_payload::<ProjectFileListPayload>(
                &self.recent,
                envelope,
                "ProjectFileList",
            ),
            FrameKind::ProjectGitStatus => parse_stream_payload::<ProjectGitStatusPayload>(
                &self.recent,
                envelope,
                "ProjectGitStatus",
            ),
            FrameKind::ProjectFileContents => parse_stream_payload::<ProjectFileContentsPayload>(
                &self.recent,
                envelope,
                "ProjectFileContents",
            ),
            FrameKind::ProjectGitDiff => parse_stream_payload::<ProjectGitDiffPayload>(
                &self.recent,
                envelope,
                "ProjectGitDiff",
            ),
            FrameKind::ProjectSearchResults => parse_stream_payload::<ProjectSearchResultsPayload>(
                &self.recent,
                envelope,
                "ProjectSearchResults",
            ),
            FrameKind::ProjectSearchComplete => {
                parse_stream_payload::<ProjectSearchCompletePayload>(
                    &self.recent,
                    envelope,
                    "ProjectSearchComplete",
                )
            }
            FrameKind::CodeIntelOverview => parse_stream_payload::<CodeIntelOverviewPayload>(
                &self.recent,
                envelope,
                "CodeIntelOverview",
            ),
            FrameKind::CodeIntelStatus => parse_stream_payload::<CodeIntelStatusPayload>(
                &self.recent,
                envelope,
                "CodeIntelStatus",
            ),
            FrameKind::CodeIntelFileModel => parse_stream_payload::<CodeIntelFileModelPayload>(
                &self.recent,
                envelope,
                "CodeIntelFileModel",
            ),
            FrameKind::CodeIntelDiagnostics => parse_stream_payload::<CodeIntelDiagnosticsPayload>(
                &self.recent,
                envelope,
                "CodeIntelDiagnostics",
            ),
            FrameKind::CodeIntelHoverResult => parse_stream_payload::<CodeIntelHoverResultPayload>(
                &self.recent,
                envelope,
                "CodeIntelHoverResult",
            ),
            FrameKind::CodeIntelNavigateResult => {
                parse_stream_payload::<CodeIntelNavigateResultPayload>(
                    &self.recent,
                    envelope,
                    "CodeIntelNavigateResult",
                )
            }
            FrameKind::CodeIntelReferencesResults => {
                parse_stream_payload::<CodeIntelReferencesResultsPayload>(
                    &self.recent,
                    envelope,
                    "CodeIntelReferencesResults",
                )
            }
            FrameKind::CodeIntelReferencesComplete => {
                parse_stream_payload::<CodeIntelReferencesCompletePayload>(
                    &self.recent,
                    envelope,
                    "CodeIntelReferencesComplete",
                )
            }
            FrameKind::CodeIntelError => parse_stream_payload::<CodeIntelErrorPayload>(
                &self.recent,
                envelope,
                "CodeIntelError",
            ),
            FrameKind::ProjectEvent => {
                parse_stream_payload::<ProjectEventPayload>(&self.recent, envelope, "ProjectEvent")
            }
            FrameKind::CommandError => {
                parse_stream_payload::<CommandErrorPayload>(&self.recent, envelope, "CommandError")
            }
            other => Err(build_violation(
                &self.recent.iter().cloned().collect::<Vec<_>>(),
                envelope,
                None,
                format!(
                    "unexpected frame kind {other} on project stream {}",
                    envelope.stream
                ),
            )),
        }
    }

    fn validate_review_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        validate_bootstrap_stream(
            &mut self.review_streams,
            &self.recent,
            envelope,
            FrameKind::ReviewBootstrap,
            "ReviewBootstrap",
        )?;
        match envelope.kind {
            FrameKind::ReviewBootstrap => parse_stream_payload::<ReviewBootstrapPayload>(
                &self.recent,
                envelope,
                "ReviewBootstrap",
            ),
            FrameKind::ReviewEvent => {
                parse_stream_payload::<ReviewEventPayload>(&self.recent, envelope, "ReviewEvent")
            }
            other => Err(build_violation(
                &self.recent.iter().cloned().collect::<Vec<_>>(),
                envelope,
                None,
                format!(
                    "unexpected frame kind {other} on review stream {}",
                    envelope.stream
                ),
            )),
        }
    }

    fn validate_browse_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        validate_bootstrap_stream(
            &mut self.browse_streams,
            &self.recent,
            envelope,
            FrameKind::BrowseBootstrap,
            "BrowseBootstrap",
        )?;
        match envelope.kind {
            FrameKind::BrowseBootstrap => parse_stream_payload::<BrowseBootstrapPayload>(
                &self.recent,
                envelope,
                "BrowseBootstrap",
            ),
            FrameKind::HostBrowseOpened => parse_stream_payload::<HostBrowseOpenedPayload>(
                &self.recent,
                envelope,
                "HostBrowseOpened",
            ),
            FrameKind::HostBrowseEntries => parse_stream_payload::<HostBrowseEntriesPayload>(
                &self.recent,
                envelope,
                "HostBrowseEntries",
            ),
            FrameKind::HostBrowseError => parse_stream_payload::<HostBrowseErrorPayload>(
                &self.recent,
                envelope,
                "HostBrowseError",
            ),
            other => Err(build_violation(
                &self.recent.iter().cloned().collect::<Vec<_>>(),
                envelope,
                None,
                format!(
                    "unexpected frame kind {other} on browse stream {}",
                    envelope.stream
                ),
            )),
        }
    }

    fn validate_terminal_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        validate_bootstrap_stream(
            &mut self.terminal_streams,
            &self.recent,
            envelope,
            FrameKind::TerminalBootstrap,
            "TerminalBootstrap",
        )?;
        match envelope.kind {
            FrameKind::TerminalBootstrap => parse_stream_payload::<TerminalBootstrapPayload>(
                &self.recent,
                envelope,
                "TerminalBootstrap",
            ),
            FrameKind::TerminalOutput => parse_stream_payload::<TerminalOutputPayload>(
                &self.recent,
                envelope,
                "TerminalOutput",
            ),
            FrameKind::TerminalExit => {
                parse_stream_payload::<TerminalExitPayload>(&self.recent, envelope, "TerminalExit")
            }
            FrameKind::TerminalError => parse_stream_payload::<TerminalErrorPayload>(
                &self.recent,
                envelope,
                "TerminalError",
            ),
            other => Err(build_violation(
                &self.recent.iter().cloned().collect::<Vec<_>>(),
                envelope,
                None,
                format!(
                    "unexpected frame kind {other} on terminal stream {}",
                    envelope.stream
                ),
            )),
        }
    }

    fn record(&mut self, envelope: &Envelope) {
        if envelope.kind == FrameKind::VoiceAudio {
            return;
        }
        let observed = ObservedFrame {
            stream: envelope.stream.clone(),
            seq: envelope.seq,
            frame_kind: envelope.kind,
            detail: summarize_envelope(envelope),
        };
        self.recent.push_back(observed);
        while self.recent.len() > self.history_limit {
            self.recent.pop_front();
        }
    }

    fn violation(
        &self,
        envelope: &Envelope,
        backend_kind: Option<BackendKind>,
        message: String,
    ) -> ProtocolViolation {
        build_violation(
            &self.recent.iter().cloned().collect::<Vec<_>>(),
            envelope,
            backend_kind,
            message,
        )
    }
}

fn validate_agent_origin(
    origin: AgentOrigin,
    parent_agent_id: Option<&crate::AgentId>,
    team_id: Option<&crate::TeamId>,
    team_member_id: Option<&crate::TeamMemberId>,
    workflow: Option<&crate::AgentWorkflowMetadata>,
) -> Result<(), String> {
    match origin {
        AgentOrigin::BackendNative if parent_agent_id.is_none() => {
            Err("backend_native agents must include parent_agent_id".to_owned())
        }
        AgentOrigin::SideQuestion if parent_agent_id.is_none() => {
            Err("side_question agents must include parent_agent_id".to_owned())
        }
        AgentOrigin::TeamMember if team_id.is_none() || team_member_id.is_none() => {
            Err("team_member agents must include team_id and team_member_id".to_owned())
        }
        AgentOrigin::Workflow if workflow.is_none() => {
            Err("workflow agents must include workflow metadata".to_owned())
        }
        AgentOrigin::User
        | AgentOrigin::AgentControl
        | AgentOrigin::SideQuestion
        | AgentOrigin::BackendNative
        | AgentOrigin::Workflow
            if team_id.is_some() || team_member_id.is_some() =>
        {
            Err("non-team_member agents must not include team_id or team_member_id".to_owned())
        }
        AgentOrigin::User
        | AgentOrigin::AgentControl
        | AgentOrigin::SideQuestion
        | AgentOrigin::BackendNative
        | AgentOrigin::TeamMember
            if workflow.is_some() =>
        {
            Err("non-workflow agents must not include workflow metadata".to_owned())
        }
        AgentOrigin::User
        | AgentOrigin::AgentControl
        | AgentOrigin::SideQuestion
        | AgentOrigin::BackendNative
        | AgentOrigin::TeamMember
        | AgentOrigin::Workflow => Ok(()),
    }
}

fn validate_spawn_agent_payload(payload: &SpawnAgentPayload) -> Result<(), String> {
    if let crate::SpawnAgentParams::Fork {
        from_session_id,
        prompt,
        images,
        ..
    } = &payload.params
    {
        if payload.parent_agent_id.is_none() {
            return Err("fork spawn_agent must include parent_agent_id".to_owned());
        }
        if from_session_id.0.trim().is_empty() {
            return Err("fork spawn_agent must include from_session_id".to_owned());
        }
        if prompt.trim().is_empty() && images.as_ref().is_none_or(|images| images.is_empty()) {
            return Err(
                "fork spawn_agent prompt must not be empty unless images are attached".to_owned(),
            );
        }
    }

    Ok(())
}

/// Wire-syntax validation for a settings write: id and operation-list bounds
/// plus RFC 6901 pointer syntax. Path semantics (schema knowledge, CAS,
/// overlap) are server concerns, not protocol validation.
fn validate_settings_write_payload(payload: &SettingsWritePayload) -> Result<(), String> {
    if payload.write_id.0.trim().is_empty() {
        return Err("settings_write write_id must not be empty".to_owned());
    }
    if payload.ops.is_empty() {
        return Err("settings_write must carry at least one operation".to_owned());
    }
    if payload.ops.len() > SETTINGS_WRITE_MAX_OPS {
        return Err(format!(
            "settings_write carries {} operations; the maximum is {}",
            payload.ops.len(),
            SETTINGS_WRITE_MAX_OPS
        ));
    }
    for op in &payload.ops {
        if parse_json_pointer(op.path()).is_none() {
            return Err(format!(
                "settings_write operation path {:?} is not a valid RFC 6901 JSON pointer",
                op.path()
            ));
        }
    }
    Ok(())
}

/// Session page descriptors are echoed back by clients as request limits, so a
/// descriptor the emitting host would itself reject is a wire defect, not a
/// client bug. Enforce the three shapes that break that contract.
///
/// Accepted, deliberately: `Complete` with no limit (a full-replay subscriber
/// returned everything in scope), `Complete` with a limit, and `More` with a
/// limit.
fn validate_session_list_page(
    validator: &ProtocolValidator,
    envelope: &Envelope,
    label: &str,
    page: &crate::SessionListPageInfo,
) -> Result<(), ProtocolViolation> {
    let max = crate::MAX_SESSION_LIST_PAGE_LIMIT;
    match (page.status, page.limit) {
        // An unbounded page returned every session in scope, so there is
        // nothing left to continue with. Advertising a continuation anyway
        // would hand "Load more" a cursor with no limit to pair it with.
        (crate::SessionListPageStatus::More { .. }, None) => Err(validator.violation(
            envelope,
            None,
            format!("{label} advertises more session pages without a page limit"),
        )),
        (_, Some(0)) => Err(validator.violation(
            envelope,
            None,
            format!("{label} session page limit must be greater than zero"),
        )),
        (_, Some(limit)) if limit > max => Err(validator.violation(
            envelope,
            None,
            format!("{label} session page limit {limit} exceeds maximum {max}"),
        )),
        _ => Ok(()),
    }
}

fn parse_host_payload<T: serde::de::DeserializeOwned>(
    validator: &ProtocolValidator,
    envelope: &Envelope,
    label: &str,
) -> Result<(), ProtocolViolation> {
    let _: T = envelope.parse_payload().map_err(|error| {
        validator.violation(
            envelope,
            None,
            format!("failed to parse {label} payload: {error}"),
        )
    })?;
    Ok(())
}

fn parse_stream_payload<T: serde::de::DeserializeOwned>(
    recent: &VecDeque<ObservedFrame>,
    envelope: &Envelope,
    label: &str,
) -> Result<(), ProtocolViolation> {
    let _: T = envelope.parse_payload().map_err(|error| {
        build_violation(
            &recent.iter().cloned().collect::<Vec<_>>(),
            envelope,
            None,
            format!("failed to parse {label} payload: {error}"),
        )
    })?;
    Ok(())
}

fn validate_bootstrap_stream(
    streams: &mut HashMap<StreamPath, BootstrapStreamState>,
    recent: &VecDeque<ObservedFrame>,
    envelope: &Envelope,
    bootstrap_kind: FrameKind,
    bootstrap_label: &str,
) -> Result<(), ProtocolViolation> {
    let recent_frames = recent.iter().cloned().collect::<Vec<_>>();
    let state = streams.entry(envelope.stream.clone()).or_default();
    if envelope.kind == bootstrap_kind {
        if state.saw_bootstrap {
            return Err(build_violation(
                &recent_frames,
                envelope,
                None,
                format!("duplicate {bootstrap_label} for stream {}", envelope.stream),
            ));
        }
        if envelope.seq != 0 {
            return Err(build_violation(
                &recent_frames,
                envelope,
                None,
                format!(
                    "{bootstrap_label} must be seq 0 on {}, got {}",
                    envelope.stream, envelope.seq
                ),
            ));
        }
        state.saw_bootstrap = true;
        return Ok(());
    }

    if !state.saw_bootstrap {
        return Err(build_violation(
            &recent_frames,
            envelope,
            None,
            format!(
                "received {} before {bootstrap_label} on {}",
                envelope.kind, envelope.stream
            ),
        ));
    }

    Ok(())
}

fn validate_agent_bootstrap_event(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    event: AgentBootstrapEvent,
) -> Result<(), ProtocolViolation> {
    match event {
        AgentBootstrapEvent::AgentStart(payload) => {
            if state.saw_agent_start {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "duplicate AgentStart inside AgentBootstrap on {}",
                        envelope.stream
                    ),
                ));
            }
            validate_agent_origin(
                payload.origin,
                payload.parent_agent_id.as_ref(),
                payload.team_id.as_ref(),
                payload.team_member_id.as_ref(),
                payload.workflow.as_ref(),
            )
            .map_err(|message| {
                build_violation(recent_frames, envelope, Some(state.backend_kind), message)
            })?;
            state.saw_agent_start = true;
            if payload.session_id.is_some() {
                state.logical_session_id = payload.session_id;
            }
            Ok(())
        }
        AgentBootstrapEvent::AgentError(_) => Ok(()),
        AgentBootstrapEvent::SessionSettings(_) => Ok(()),
        AgentBootstrapEvent::QueuedMessages(_) => Ok(()),
        AgentBootstrapEvent::AgentActivityStats(payload) => {
            if !state.saw_agent_start {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "AgentActivityStats arrived before AgentStart inside {}",
                        envelope.kind
                    ),
                ));
            }
            if payload.agent_id != state.agent_id {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "AgentActivityStats agent_id {} does not match stream agent_id {}",
                        payload.agent_id, state.agent_id
                    ),
                ));
            }
            Ok(())
        }
        AgentBootstrapEvent::ContextCompaction(payload) => {
            if payload.status.is_terminal() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "terminal ContextCompaction snapshot must not be bootstrapped".to_owned(),
                ));
            }
            validate_context_compaction_notify(recent_frames, envelope, state, &payload)
        }
        AgentBootstrapEvent::ContextCompactionCapability(payload) => {
            if payload.agent_id != state.agent_id {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "ContextCompactionCapability agent_id {} does not match stream agent_id {}",
                        payload.agent_id, state.agent_id
                    ),
                ));
            }
            Ok(())
        }
        AgentBootstrapEvent::HasPriorHistory { .. } => Ok(()),
        AgentBootstrapEvent::ChatEvent(event) => {
            if !state.saw_agent_start {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "ChatEvent arrived before AgentStart inside {}",
                        envelope.kind
                    ),
                ));
            }
            validate_chat_event(recent_frames, envelope, state, &event)
        }
    }
}

fn validate_chat_event(
    _recent_frames: &[ObservedFrame],
    _envelope: &Envelope,
    _state: &mut AgentStreamState,
    _event: &ChatEvent,
) -> Result<(), ProtocolViolation> {
    Ok(())
}

fn validate_context_compaction_notify(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    payload: &ContextCompactionNotifyPayload,
) -> Result<(), ProtocolViolation> {
    if payload.agent_id != state.agent_id {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "ContextCompactionNotify agent_id {} does not match stream agent_id {}",
                payload.agent_id, state.agent_id
            ),
        ));
    }
    if payload.logical_session_id.0.trim().is_empty() {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            "ContextCompactionNotify has an empty logical_session_id".to_owned(),
        ));
    }
    if let Some(logical_session_id) = state.logical_session_id.as_ref()
        && &payload.logical_session_id != logical_session_id
    {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "ContextCompactionNotify logical_session_id {} does not match current session {}",
                payload.logical_session_id.0, logical_session_id.0
            ),
        ));
    }

    let terminal = payload.status.is_terminal();
    match state.compaction_operations.get(&payload.operation_id) {
        Some(true) => Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "ContextCompactionNotify arrived after terminal status for operation {}",
                payload.operation_id.0
            ),
        )),
        Some(false) | None => {
            state
                .compaction_operations
                .insert(payload.operation_id.clone(), terminal);
            Ok(())
        }
    }
}

fn build_violation(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    backend_kind: Option<BackendKind>,
    message: String,
) -> ProtocolViolation {
    ProtocolViolation {
        stream: envelope.stream.clone(),
        seq: envelope.seq,
        frame_kind: envelope.kind,
        backend_kind,
        message,
        recent_frames: recent_frames.to_vec(),
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolViolation {
    pub stream: StreamPath,
    pub seq: u64,
    pub frame_kind: FrameKind,
    pub backend_kind: Option<BackendKind>,
    pub message: String,
    pub recent_frames: Vec<ObservedFrame>,
}

impl fmt::Display for ProtocolViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = self
            .backend_kind
            .map(|kind| format!("{kind:?}"))
            .unwrap_or_else(|| "unknown".to_owned());

        writeln!(
            f,
            "{} on stream {} seq {} kind {} backend {}",
            self.message, self.stream, self.seq, self.frame_kind, backend
        )?;
        writeln!(f, "recent frames:")?;
        for frame in &self.recent_frames {
            writeln!(
                f,
                "  seq={} stream={} kind={} {}",
                frame.seq, frame.stream, frame.frame_kind, frame.detail
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ProtocolViolation {}

#[derive(Debug, Clone)]
pub struct ObservedFrame {
    pub stream: StreamPath,
    pub seq: u64,
    pub frame_kind: FrameKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct HostStreamState {
    saw_welcome: bool,
    saw_bootstrap: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct BootstrapStreamState {
    saw_bootstrap: bool,
}

#[derive(Debug, Clone)]
struct AgentStreamState {
    agent_id: crate::AgentId,
    backend_kind: BackendKind,
    logical_session_id: Option<crate::SessionId>,
    saw_bootstrap: bool,
    saw_agent_start: bool,
    compaction_operations: HashMap<crate::CompactionOperationId, bool>,
}

fn summarize_envelope(envelope: &Envelope) -> String {
    if envelope.kind != FrameKind::ChatEvent {
        return String::new();
    }

    match envelope.parse_payload::<ChatEvent>() {
        Ok(event) => summarize_chat_event(&event),
        Err(error) => format!("payload_parse_error={error}"),
    }
}

fn summarize_chat_event(event: &ChatEvent) -> String {
    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            format!("event=typing_status_changed typing={typing}")
        }
        ChatEvent::MessageAdded(message) => {
            format!("event=message_added sender={:?}", message.sender)
        }
        ChatEvent::MessageMetadataUpdated(data) => format!(
            "event=message_metadata_updated message_id={}",
            data.message_id
        ),
        ChatEvent::StreamStart(data) => {
            format!("event=stream_start agent={:?}", data.agent)
        }
        ChatEvent::StreamDelta(data) => {
            format!("event=stream_delta text_len={}", data.text.len())
        }
        ChatEvent::StreamReasoningDelta(data) => {
            format!("event=stream_reasoning_delta text_len={}", data.text.len())
        }
        ChatEvent::StreamEnd(data) => format!(
            "event=stream_end sender={:?} text_len={}",
            data.message.sender,
            data.message.content.len()
        ),
        ChatEvent::ToolRequest(data) => {
            format!("event=tool_request tool_call_id={}", data.tool_call_id)
        }
        ChatEvent::ToolProgress(data) => format!(
            "event=tool_progress tool_call_id={} mode={:?}",
            data.tool_call_id, data.execution_mode
        ),
        ChatEvent::ToolExecutionCompleted(data) => format!(
            "event=tool_execution_completed tool_call_id={} outcome={:?}",
            data.tool_call_id, data.outcome
        ),
        ChatEvent::TaskUpdate(tasks) => {
            format!(
                "event=task_update title={:?} tasks={}",
                tasks.title,
                tasks.tasks.len()
            )
        }
        ChatEvent::OperationCancelled(data) => {
            format!("event=operation_cancelled message={:?}", data.message)
        }
        ChatEvent::RetryAttempt(data) => {
            format!(
                "event=retry_attempt attempt={} max={}",
                data.attempt, data.max_retries
            )
        }
        ChatEvent::Orchestration(data) => format!(
            "event=orchestration agent_id={} agent_type={} payload={}",
            data.agent_id,
            data.agent_type,
            data.payload.kind()
        ),
        ChatEvent::ContextCompaction(data) => format!(
            "event=context_compaction marker_id={} operation_id={:?} status={:?} mutation={:?}",
            data.marker_id.0, data.operation_id, data.status, data.mutation
        ),
    }
}
