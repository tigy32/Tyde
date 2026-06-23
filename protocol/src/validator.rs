use std::collections::{HashMap, VecDeque};
use std::fmt;

use crate::types::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentCompactNotifyPayload, AgentCompactPayload,
    BrowseBootstrapPayload, CloseAgentPayload, NewTerminalPayload, ProjectBootstrapPayload,
    ReviewBootstrapPayload, TeamCompactNotifyPayload, TeamCompactPayload, TerminalBootstrapPayload,
};
use crate::{
    AgentClosedPayload, AgentOrigin, AgentStartPayload, BackendKind, BackendSetupPayload,
    CancelWorkflowPayload, ChatEvent, ChatMessage, ChatMessageId, ClientErrorPayload,
    CommandErrorPayload, CustomAgentDeletePayload, CustomAgentNotifyPayload,
    CustomAgentUpsertPayload, DeleteSessionPayload, Envelope, FrameKind, HostBootstrapPayload,
    HostBrowseClosePayload, HostBrowseEntriesPayload, HostBrowseErrorPayload,
    HostBrowseListPayload, HostBrowseOpenedPayload, HostBrowseStartPayload, HostSettingsPayload,
    ListSessionsPayload, LoadAgentPayload, McpServerDeletePayload, McpServerNotifyPayload,
    McpServerUpsertPayload, MobileAccessStatePayload, MobileDeviceRenamePayload,
    MobileDeviceRevokePayload, MobilePairingCancelPayload, MobilePairingOfferPayload,
    MobilePairingStartPayload, NewAgentPayload, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDeleteRootPayload, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectNotifyPayload, ProjectRenamePayload, ProjectReorderPayload,
    ProjectSearchCompletePayload, ProjectSearchResultsPayload, ReviewEventPayload,
    RunBackendSetupPayload, SessionListPayload, SessionSchemasPayload, SetSettingPayload,
    SkillNotifyPayload, SkillRefreshPayload, SpawnAgentPayload, SteeringDeletePayload,
    SteeringNotifyPayload, SteeringUpsertPayload, StreamPath, TeamCreatePayload, TeamDeletePayload,
    TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftDiscardPayload, TeamDraftNotifyPayload, TeamDraftShufflePayload,
    TeamDraftUpdatePayload, TeamMemberActivatePayload, TeamMemberBindingNotifyPayload,
    TeamMemberCreatePayload, TeamMemberDeletePayload, TeamMemberNotifyPayload,
    TeamMemberShufflePayload, TeamMemberShuffleSuggestionNotifyPayload, TeamMemberUpdatePayload,
    TeamNotifyPayload, TeamPresetCatalogNotifyPayload, TeamRenamePayload, TeamSetManagerPayload,
    TerminalCreatePayload, TerminalErrorPayload, TerminalExitPayload, TerminalOutputPayload,
    ToolExecutionCompletedData, ToolRequest, TriggerWorkflowPayload, WelcomePayload,
    WorkbenchCreatePayload, WorkbenchRemovePayload, WorkflowNotifyPayload, WorkflowRefreshPayload,
    WorkflowRunNotifyPayload,
};

const DEFAULT_HISTORY_LIMIT: usize = 32;

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

        Ok(())
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
            FrameKind::SessionSchemas => {
                parse_host_payload::<SessionSchemasPayload>(self, envelope, "SessionSchemas")
            }
            FrameKind::SessionList => {
                parse_host_payload::<SessionListPayload>(self, envelope, "SessionList")
            }
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
            FrameKind::SetSetting => {
                parse_host_payload::<SetSettingPayload>(self, envelope, "SetSetting")
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
            FrameKind::ClientError => {
                parse_host_payload::<ClientErrorPayload>(self, envelope, "ClientError")
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
                for event in payload.events {
                    validate_agent_bootstrap_event(&recent_frames, envelope, state, event)?;
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
                saw_bootstrap: false,
                saw_agent_start: false,
                active_stream: None,
                assistant_turn_open: false,
                known_message_ids: HashMap::new(),
                pending_tool_calls: HashMap::new(),
                cancelled_tool_calls: HashMap::new(),
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
            Ok(())
        }
        AgentBootstrapEvent::AgentError(_) => Ok(()),
        AgentBootstrapEvent::SessionSettings(_) => Ok(()),
        AgentBootstrapEvent::QueuedMessages(_) => Ok(()),
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
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    event: &ChatEvent,
) -> Result<(), ProtocolViolation> {
    match event {
        ChatEvent::MessageAdded(message) => {
            register_message_id(recent_frames, envelope, state, message)?;
            match &message.sender {
                crate::MessageSender::Assistant { .. } => {
                    if !state.pending_tool_calls.is_empty() {
                        return Err(build_violation(
                            recent_frames,
                            envelope,
                            Some(state.backend_kind),
                            "received assistant MessageAdded while previous tool requests are still unresolved"
                                .to_owned(),
                        ));
                    }
                    state.assistant_turn_open = true;
                    Ok(())
                }
                _ => {
                    state.assistant_turn_open = false;
                    Ok(())
                }
            }
        }
        ChatEvent::MessageMetadataUpdated(update) => {
            match state.known_message_ids.get(&update.message_id) {
                Some(KnownMessageKind::Assistant) => Ok(()),
                Some(KnownMessageKind::Other) => Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "received MessageMetadataUpdated for non-assistant message_id {}",
                        update.message_id
                    ),
                )),
                None => Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "received MessageMetadataUpdated for unknown message_id {}",
                        update.message_id
                    ),
                )),
            }
        }
        ChatEvent::StreamStart(data) => {
            if state.active_stream.is_some() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamStart while previous assistant stream is still open".to_owned(),
                ));
            }
            if !state.pending_tool_calls.is_empty() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamStart while previous tool requests are still unresolved"
                        .to_owned(),
                ));
            }
            state.assistant_turn_open = true;
            state.active_stream = Some(ActiveStreamState {
                message_id: data.message_id.clone(),
            });
            Ok(())
        }
        ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
            let Some(active) = state.active_stream.as_mut() else {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!("received {} before StreamStart", chat_event_label(event)),
                ));
            };
            if let Some(actual) = &delta.message_id {
                active.message_id = Some(actual.clone());
            }
            Ok(())
        }
        ChatEvent::StreamEnd(data) => {
            let Some(active_stream) = state.active_stream.take() else {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamEnd before StreamStart".to_owned(),
                ));
            };
            if let (Some(expected), Some(actual)) = (
                active_stream.message_id.as_deref(),
                data.message.message_id.as_ref().map(|id| id.0.as_str()),
            ) && expected != actual
            {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "received StreamEnd message_id {actual} but active stream message_id is {expected}"
                    ),
                ));
            }
            register_message_id(recent_frames, envelope, state, &data.message)?;
            Ok(())
        }
        ChatEvent::ToolRequest(request) => {
            validate_tool_request(recent_frames, envelope, state, request)
        }
        ChatEvent::ToolExecutionCompleted(data) => {
            validate_tool_execution_completed(recent_frames, envelope, state, data)
        }
        // Progress is legal at any point relative to its tool call —
        // background tasks emit progress after the tool result and across
        // turn boundaries — so the only requirement is a non-empty id.
        ChatEvent::ToolProgress(data) => {
            if data.tool_call_id.is_empty() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received ToolProgress with empty tool_call_id".to_owned(),
                ));
            }
            Ok(())
        }
        ChatEvent::OperationCancelled(_) => {
            state
                .cancelled_tool_calls
                .extend(state.pending_tool_calls.drain());
            state.assistant_turn_open = false;
            Ok(())
        }
        ChatEvent::TypingStatusChanged(_)
        | ChatEvent::TaskUpdate(_)
        | ChatEvent::RetryAttempt(_) => Ok(()),
    }
}

fn register_message_id(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    message: &ChatMessage,
) -> Result<(), ProtocolViolation> {
    let Some(message_id) = &message.message_id else {
        return Ok(());
    };
    let kind = match &message.sender {
        crate::MessageSender::Assistant { .. } => KnownMessageKind::Assistant,
        _ => KnownMessageKind::Other,
    };
    if let Some(existing) = state.known_message_ids.get(message_id)
        && *existing != kind
    {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "received message_id {} for {:?} message after it was used for {:?}",
                message_id, kind, existing
            ),
        ));
    }
    state.known_message_ids.insert(message_id.clone(), kind);
    Ok(())
}

fn validate_tool_request(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    request: &ToolRequest,
) -> Result<(), ProtocolViolation> {
    if !state.assistant_turn_open {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "received ToolRequest {} before any assistant turn",
                request.tool_call_id
            ),
        ));
    }

    if state
        .pending_tool_calls
        .insert(request.tool_call_id.clone(), request.tool_name.clone())
        .is_some()
    {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "duplicate ToolRequest for tool_call_id {}",
                request.tool_call_id
            ),
        ));
    }
    Ok(())
}

fn validate_tool_execution_completed(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    data: &ToolExecutionCompletedData,
) -> Result<(), ProtocolViolation> {
    let expected_tool_name = state
        .pending_tool_calls
        .remove(&data.tool_call_id)
        .or_else(|| state.cancelled_tool_calls.remove(&data.tool_call_id));
    let Some(expected_tool_name) = expected_tool_name else {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "received ToolExecutionCompleted for unknown tool_call_id {}",
                data.tool_call_id
            ),
        ));
    };

    if expected_tool_name != data.tool_name {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "tool completion name mismatch for {}: expected {:?}, got {:?}",
                data.tool_call_id, expected_tool_name, data.tool_name
            ),
        ));
    }

    Ok(())
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
    saw_bootstrap: bool,
    saw_agent_start: bool,
    active_stream: Option<ActiveStreamState>,
    assistant_turn_open: bool,
    known_message_ids: HashMap<ChatMessageId, KnownMessageKind>,
    pending_tool_calls: HashMap<String, String>,
    cancelled_tool_calls: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct ActiveStreamState {
    message_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownMessageKind {
    Assistant,
    Other,
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
        ChatEvent::StreamStart(data) => format!(
            "event=stream_start message_id={:?} agent={:?}",
            data.message_id, data.agent
        ),
        ChatEvent::StreamDelta(data) => format!(
            "event=stream_delta message_id={:?} text_len={}",
            data.message_id,
            data.text.len()
        ),
        ChatEvent::StreamReasoningDelta(data) => format!(
            "event=stream_reasoning_delta message_id={:?} text_len={}",
            data.message_id,
            data.text.len()
        ),
        ChatEvent::StreamEnd(data) => format!(
            "event=stream_end sender={:?} text_len={}",
            data.message.sender,
            data.message.content.len()
        ),
        ChatEvent::ToolRequest(data) => format!(
            "event=tool_request tool_call_id={} tool_name={}",
            data.tool_call_id, data.tool_name
        ),
        ChatEvent::ToolProgress(data) => format!(
            "event=tool_progress tool_call_id={} tool_name={}",
            data.tool_call_id, data.tool_name
        ),
        ChatEvent::ToolExecutionCompleted(data) => format!(
            "event=tool_execution_completed tool_call_id={} tool_name={} success={}",
            data.tool_call_id, data.tool_name, data.success
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
    }
}

fn chat_event_label(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::TypingStatusChanged(_) => "TypingStatusChanged",
        ChatEvent::MessageAdded(_) => "MessageAdded",
        ChatEvent::MessageMetadataUpdated(_) => "MessageMetadataUpdated",
        ChatEvent::StreamStart(_) => "StreamStart",
        ChatEvent::StreamDelta(_) => "StreamDelta",
        ChatEvent::StreamReasoningDelta(_) => "StreamReasoningDelta",
        ChatEvent::StreamEnd(_) => "StreamEnd",
        ChatEvent::ToolRequest(_) => "ToolRequest",
        ChatEvent::ToolProgress(_) => "ToolProgress",
        ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted",
        ChatEvent::TaskUpdate(_) => "TaskUpdate",
        ChatEvent::OperationCancelled(_) => "OperationCancelled",
        ChatEvent::RetryAttempt(_) => "RetryAttempt",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ChatMessage, ChatMessageId, MessageMetadataUpdateData, MessageSender, ModelInfo,
        StreamEndData, StreamStartData, StreamTextDeltaData, TokenUsage,
    };

    fn host_stream() -> StreamPath {
        StreamPath("/host/test".to_owned())
    }

    fn agent_stream() -> StreamPath {
        StreamPath("/agent/test-agent".to_owned())
    }

    fn new_agent_payload(
        origin: AgentOrigin,
        team_id: Option<crate::TeamId>,
        team_member_id: Option<crate::TeamMemberId>,
    ) -> NewAgentPayload {
        NewAgentPayload {
            agent_id: crate::AgentId("test-agent".to_owned()),
            name: "test".to_owned(),
            origin,
            backend_kind: BackendKind::Claude,
            workspace_roots: vec![],
            custom_agent_id: None,
            team_id,
            team_member_id,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: agent_stream(),
        }
    }

    fn host_bootstrap_with_agents(agents: Vec<NewAgentPayload>) -> Envelope {
        Envelope::from_payload(
            host_stream(),
            FrameKind::HostBootstrap,
            0,
            &HostBootstrapPayload {
                settings: crate::HostSettings {
                    enabled_backends: vec![],
                    default_backend: None,
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                },
                mobile_access: MobileAccessStatePayload {
                    broker_status: crate::MobileBrokerStatus::Disabled,
                    pairing: crate::MobilePairingState::Idle,
                    paired_devices: vec![],
                },
                backend_setup: BackendSetupPayload { backends: vec![] },
                session_schemas: vec![],
                sessions: vec![],
                projects: vec![],
                mcp_servers: vec![],
                skills: vec![],
                steering: vec![],
                custom_agents: vec![],
                team_preset_catalog: crate::TeamPresetCatalog {
                    role_presets: vec![],
                    personality_traits: vec![],
                    personality_presets: vec![],
                    team_templates: vec![],
                },
                team_drafts: vec![],
                teams: vec![],
                team_members: vec![],
                team_member_bindings: vec![],
                agents,
                workflow_summaries: vec![],
                workflow_diagnostics: vec![],
                workflow_runs: vec![],
            },
        )
        .expect("serialize HostBootstrap")
    }

    fn new_agent_envelope() -> Envelope {
        host_bootstrap_with_agents(vec![new_agent_payload(AgentOrigin::User, None, None)])
    }

    fn agent_start_payload() -> crate::AgentStartPayload {
        crate::AgentStartPayload {
            agent_id: crate::AgentId("test-agent".to_owned()),
            name: "test".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: vec![],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
        }
    }

    fn agent_bootstrap_start_envelope() -> Envelope {
        Envelope::from_payload(
            agent_stream(),
            FrameKind::AgentBootstrap,
            0,
            &AgentBootstrapPayload {
                events: vec![AgentBootstrapEvent::AgentStart(agent_start_payload())],
            },
        )
        .expect("serialize AgentBootstrap")
    }

    fn chat_envelope(seq: u64, event: &ChatEvent) -> Envelope {
        Envelope::from_payload(agent_stream(), FrameKind::ChatEvent, seq, event)
            .expect("serialize ChatEvent")
    }

    fn new_agent_with_team_fields(
        origin: AgentOrigin,
        team_id: Option<crate::TeamId>,
        team_member_id: Option<crate::TeamMemberId>,
    ) -> Envelope {
        host_bootstrap_with_agents(vec![new_agent_payload(origin, team_id, team_member_id)])
    }

    fn assistant_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 0,
            sender: MessageSender::Assistant {
                agent: "assistant".to_owned(),
            },
            content: content.to_owned(),
            reasoning: None,
            tool_calls: vec![],
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn assistant_message_with_id(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            message_id: Some(ChatMessageId(id.to_owned())),
            ..assistant_message(content)
        }
    }

    fn assistant_message_added(content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(assistant_message(content))
    }

    fn assistant_message_added_with_id(id: &str, content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(assistant_message_with_id(id, content))
    }

    fn user_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 0,
            sender: MessageSender::User,
            content: content.to_owned(),
            reasoning: None,
            tool_calls: vec![],
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn user_message_added(content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(user_message(content))
    }

    fn user_message_added_with_id(id: &str, content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(ChatMessage {
            message_id: Some(ChatMessageId(id.to_owned())),
            ..user_message(content)
        })
    }

    fn metadata_updated(message_id: &str) -> ChatEvent {
        ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
            message_id: ChatMessageId(message_id.to_owned()),
            model_info: Some(ModelInfo {
                model: "gpt-5-codex".to_owned(),
            }),
            token_usage: Some(TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cached_prompt_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                reasoning_tokens: Some(0),
            }),
            context_breakdown: None,
        })
    }

    fn tool_request(call_id: &str) -> ChatEvent {
        ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_type: crate::ToolRequestType::Other { args: json!({}) },
        })
    }

    fn tool_completed(call_id: &str) -> ChatEvent {
        ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
            tool_call_id: call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_result: crate::ToolExecutionResult::Other { result: json!({}) },
            success: true,
            error: None,
        })
    }

    #[test]
    fn host_bootstrap_after_welcome_registers_agent_streams() {
        let mut validator = ProtocolValidator::new();
        let welcome = Envelope::from_payload(
            host_stream(),
            FrameKind::Welcome,
            0,
            &crate::WelcomePayload {
                protocol_version: crate::PROTOCOL_VERSION,
                tyde_version: crate::TYDE_VERSION,
                release_version: None,
            },
        )
        .expect("serialize Welcome");
        let mut bootstrap = new_agent_envelope();
        bootstrap.seq = 1;

        validator.validate_envelope(&welcome).unwrap();
        validator.validate_envelope(&bootstrap).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
    }

    #[test]
    fn post_handshake_host_bootstrap_registers_agent_streams() {
        let mut validator = ProtocolValidator::new();
        let mut bootstrap = new_agent_envelope();
        bootstrap.seq = 1;

        validator.validate_envelope(&bootstrap).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
    }

    #[test]
    fn rejects_host_replay_before_host_bootstrap() {
        let mut validator = ProtocolValidator::new();
        let envelope = Envelope::from_payload(
            host_stream(),
            FrameKind::HostSettings,
            0,
            &HostSettingsPayload {
                settings: crate::HostSettings {
                    enabled_backends: vec![],
                    default_backend: None,
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                },
            },
        )
        .expect("serialize HostSettings");
        let violation = validator
            .validate_envelope(&envelope)
            .expect_err("HostSettings before HostBootstrap should be invalid");

        assert!(violation.to_string().contains("before HostBootstrap"));
    }

    #[test]
    fn rejects_agent_event_before_agent_bootstrap() {
        let mut validator = ProtocolValidator::new();
        validator.validate_envelope(&new_agent_envelope()).unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(0, &assistant_message_added("hi")))
            .expect_err("agent events before AgentBootstrap should be invalid");

        assert!(violation.to_string().contains("before AgentBootstrap"));
    }

    #[test]
    fn rejects_project_event_before_project_bootstrap() {
        let mut validator = ProtocolValidator::new();
        let envelope = Envelope::from_payload(
            StreamPath("/project/test".to_owned()),
            FrameKind::ProjectEvent,
            0,
            &crate::ProjectEventPayload::ReviewListChanged { reviews: vec![] },
        )
        .expect("serialize ProjectEvent");
        let violation = validator
            .validate_envelope(&envelope)
            .expect_err("ProjectEvent before ProjectBootstrap should be invalid");

        assert!(violation.to_string().contains("before ProjectBootstrap"));
    }

    #[test]
    fn accepts_workspace_review_summary_scope_payloads() {
        let mut validator = ProtocolValidator::new();
        let stream = StreamPath("/project/project-1".to_owned());
        let summary = crate::ReviewSummary {
            id: crate::ReviewId("review-1".to_owned()),
            scope: crate::ReviewSummaryScope::Workspace,
            status: crate::ReviewStatus::Draft,
            origin_session_id: crate::SessionId("session-1".to_owned()),
            origin_agent_id: crate::AgentId("agent-1".to_owned()),
            created_at_ms: 1,
            updated_at_ms: 2,
            user_comment_count: 1,
            pending_suggestion_count: 0,
            file_comment_counts: vec![crate::ReviewFileCommentCount {
                root: crate::ProjectRootPath("/repo-a".to_owned()),
                relative_path: "src/lib.rs".to_owned(),
                user_comment_count: 1,
                ai_comment_count: 0,
                pending_suggestion_count: 0,
            }],
        };
        let bootstrap = Envelope::from_payload(
            stream.clone(),
            FrameKind::ProjectBootstrap,
            0,
            &crate::ProjectBootstrapPayload {
                project: crate::Project {
                    id: crate::ProjectId("project-1".to_owned()),
                    name: "Project".to_owned(),
                    sort_order: 0,
                    source: crate::ProjectSource::Standalone {
                        roots: vec![
                            crate::ProjectRootPath("/repo-a".to_owned()),
                            crate::ProjectRootPath("/repo-b".to_owned()),
                        ],
                    },
                },
                file_list: crate::ProjectFileListPayload {
                    incremental: false,
                    roots: vec![],
                },
                git_status: crate::ProjectGitStatusPayload { roots: vec![] },
                review_summaries: vec![summary.clone()],
            },
        )
        .expect("serialize ProjectBootstrap");
        validator.validate_envelope(&bootstrap).unwrap();

        let event = Envelope::from_payload(
            stream,
            FrameKind::ProjectEvent,
            1,
            &crate::ProjectEventPayload::ReviewListChanged {
                reviews: vec![summary],
            },
        )
        .expect("serialize ProjectEvent");
        validator.validate_envelope(&event).unwrap();
    }

    #[test]
    fn accepts_team_member_origin_with_team_fields() {
        let mut validator = ProtocolValidator::new();
        validator
            .validate_envelope(&new_agent_with_team_fields(
                AgentOrigin::TeamMember,
                Some(crate::TeamId("team-1".to_owned())),
                Some(crate::TeamMemberId("member-1".to_owned())),
            ))
            .unwrap();
    }

    #[test]
    fn rejects_team_member_origin_without_team_fields() {
        let mut validator = ProtocolValidator::new();
        let violation = validator
            .validate_envelope(&new_agent_with_team_fields(
                AgentOrigin::TeamMember,
                Some(crate::TeamId("team-1".to_owned())),
                None,
            ))
            .expect_err("team member origin should require both team ids");

        assert!(violation.to_string().contains("team_id and team_member_id"));
    }

    #[test]
    fn rejects_non_team_origin_with_team_fields() {
        let mut validator = ProtocolValidator::new();
        let violation = validator
            .validate_envelope(&new_agent_with_team_fields(
                AgentOrigin::User,
                Some(crate::TeamId("team-1".to_owned())),
                Some(crate::TeamMemberId("member-1".to_owned())),
            ))
            .expect_err("user origin should not include team ids");

        assert!(violation.to_string().contains("non-team_member"));
    }

    #[test]
    fn accepts_workflow_origin_with_metadata() {
        let mut payload = new_agent_payload(AgentOrigin::Workflow, None, None);
        payload.workflow = Some(crate::AgentWorkflowMetadata {
            workflow_id: crate::WorkflowId("build".to_owned()),
            workflow_run_id: crate::WorkflowRunId("run-1".to_owned()),
        });

        let mut validator = ProtocolValidator::new();
        validator
            .validate_envelope(&host_bootstrap_with_agents(vec![payload]))
            .unwrap();
    }

    #[test]
    fn rejects_workflow_origin_without_metadata() {
        let mut validator = ProtocolValidator::new();
        let violation = validator
            .validate_envelope(&host_bootstrap_with_agents(vec![new_agent_payload(
                AgentOrigin::Workflow,
                None,
                None,
            )]))
            .expect_err("workflow origin should require workflow metadata");

        assert!(violation.to_string().contains("workflow metadata"));
    }

    #[test]
    fn rejects_non_workflow_origin_with_workflow_metadata() {
        let mut payload = new_agent_payload(AgentOrigin::User, None, None);
        payload.workflow = Some(crate::AgentWorkflowMetadata {
            workflow_id: crate::WorkflowId("build".to_owned()),
            workflow_run_id: crate::WorkflowRunId("run-1".to_owned()),
        });

        let mut validator = ProtocolValidator::new();
        let violation = validator
            .validate_envelope(&host_bootstrap_with_agents(vec![payload]))
            .expect_err("user origin should reject workflow metadata");

        assert!(violation.to_string().contains("non-workflow"));
    }

    #[test]
    fn rejects_side_question_origin_without_parent() {
        let mut validator = ProtocolValidator::new();
        let violation = validator
            .validate_envelope(&host_bootstrap_with_agents(vec![new_agent_payload(
                AgentOrigin::SideQuestion,
                None,
                None,
            )]))
            .expect_err("side_question origin should require a parent agent");

        assert!(violation.to_string().contains("parent_agent_id"));
    }

    #[test]
    fn accepts_side_question_origin_with_parent() {
        let mut payload = new_agent_payload(AgentOrigin::SideQuestion, None, None);
        payload.parent_agent_id = Some(crate::AgentId("parent-agent".to_owned()));

        let mut validator = ProtocolValidator::new();
        validator
            .validate_envelope(&host_bootstrap_with_agents(vec![payload]))
            .unwrap();
    }

    #[test]
    fn rejects_fork_spawn_without_parent_agent_id() {
        let envelope = Envelope::from_payload(
            host_stream(),
            FrameKind::SpawnAgent,
            1,
            &crate::SpawnAgentPayload {
                name: None,
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: crate::SpawnAgentParams::Fork {
                    from_session_id: crate::SessionId("parent-session".to_owned()),
                    prompt: "side question".to_owned(),
                    images: None,
                    access_mode: None,
                },
            },
        )
        .expect("serialize SpawnAgent");

        let mut validator = ProtocolValidator::new();
        validator
            .validate_envelope(&host_bootstrap_with_agents(vec![]))
            .unwrap();
        let violation = validator
            .validate_envelope(&envelope)
            .expect_err("fork spawn should require parent_agent_id");

        assert!(violation.to_string().contains("parent_agent_id"));
    }

    #[test]
    fn rejects_fork_spawn_without_from_session_id() {
        let envelope = Envelope::from_payload(
            host_stream(),
            FrameKind::SpawnAgent,
            1,
            &crate::SpawnAgentPayload {
                name: None,
                custom_agent_id: None,
                parent_agent_id: Some(crate::AgentId("parent-agent".to_owned())),
                project_id: None,
                params: crate::SpawnAgentParams::Fork {
                    from_session_id: crate::SessionId(String::new()),
                    prompt: "side question".to_owned(),
                    images: None,
                    access_mode: None,
                },
            },
        )
        .expect("serialize SpawnAgent");

        let mut validator = ProtocolValidator::new();
        validator
            .validate_envelope(&host_bootstrap_with_agents(vec![]))
            .unwrap();
        let violation = validator
            .validate_envelope(&envelope)
            .expect_err("fork spawn should require from_session_id");

        assert!(violation.to_string().contains("from_session_id"));
    }

    #[test]
    fn accepts_turn_with_tools_after_stream_end() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("msg-1".to_owned()),
                    text: "hi".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                3,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn accepts_non_streaming_turn_with_tools_after_assistant_message() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("hi")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn accepts_metadata_update_after_known_assistant_message_id() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &assistant_message_added_with_id("msg-1", "hi"),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &metadata_updated("msg-1")))
            .unwrap();
    }

    #[test]
    fn rejects_metadata_update_for_unknown_message_id() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(1, &metadata_updated("missing-msg")))
            .expect_err("metadata update without a known message id should be invalid");

        assert!(violation.to_string().contains("unknown message_id"));
    }

    #[test]
    fn rejects_metadata_update_for_non_assistant_message_id() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &user_message_added_with_id("user-msg", "hi"),
            ))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(2, &metadata_updated("user-msg")))
            .expect_err("metadata update for a user message should be invalid");

        assert!(violation.to_string().contains("non-assistant message_id"));
    }

    #[test]
    fn rejects_stream_end_message_id_mismatch() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message_with_id("msg-2", "hi"),
                }),
            ))
            .expect_err("StreamEnd should preserve the active stream message id");

        assert!(violation.to_string().contains("active stream message_id"));
    }

    #[test]
    fn accepts_streaming_turn_with_tool_request_before_stream_end() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                3,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn rejects_tool_request_before_assistant_turn() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(1, &tool_request("call-1")))
            .expect_err("tool request before assistant turn should be invalid");

        assert!(violation.to_string().contains("ToolRequest"));
        assert_eq!(violation.backend_kind, Some(BackendKind::Claude));
    }

    #[test]
    fn rejects_stream_delta_before_stream_start() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("msg-1".to_owned()),
                    text: "hi".to_owned(),
                }),
            ))
            .expect_err("delta before stream start should be invalid");

        assert!(
            violation
                .to_string()
                .contains("StreamDelta before StreamStart")
        );
    }

    #[test]
    fn rejects_next_turn_when_tool_request_is_unresolved() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-2".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .expect_err("next turn should not start while tool request is unresolved");

        assert!(
            violation
                .to_string()
                .contains("previous tool requests are still unresolved")
        );
    }

    #[test]
    fn operation_cancelled_clears_unresolved_tool_requests() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::OperationCancelled(crate::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                5,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-2".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
    }

    #[test]
    fn accepts_late_tool_completion_after_operation_cancelled() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::OperationCancelled(crate::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn rejects_unknown_tool_completion_even_after_assistant_turn() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("hi")))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(2, &tool_completed("call-unknown")))
            .expect_err("unknown tool completion should be invalid");

        assert!(violation.to_string().contains("unknown tool_call_id"));
        assert_eq!(violation.backend_kind, Some(BackendKind::Claude));
    }

    #[test]
    fn accepts_mixed_non_streaming_sequence_across_multiple_turns() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_bootstrap_start_envelope())
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_completed("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &user_message_added("next turn")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &assistant_message_added("")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(6, &tool_request("call-2")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(7, &tool_completed("call-2")))
            .unwrap();
    }
}
