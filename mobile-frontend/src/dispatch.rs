use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use protocol::MobileAccessErrorCode;
use protocol::types::{AgentCompactNotifyPayload, AgentCompactStatus};
use protocol::{
    AgentClosedPayload, AgentErrorPayload, AgentId, AgentOrigin, AgentRenamedPayload,
    AgentStartPayload, BackendSetupPayload, ChatEvent, CommandErrorPayload,
    CustomAgentNotifyPayload, Envelope, FrameKind, HostBrowseEntriesPayload,
    HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload, ListSessionsPayload,
    McpServerNotifyPayload, NewAgentPayload, ProjectEventPayload, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, ProtocolValidator, QueuedMessagesPayload, RejectPayload,
    ReviewEventPayload, ReviewId, SeqMismatch, SessionListPayload, SessionSchemasPayload,
    SessionSettingsPayload, SkillNotifyPayload, SteeringNotifyPayload, StreamPath,
    TeamCompactNotifyPayload, TeamCompactStatus, TeamDraftNotifyPayload,
    TeamMemberBindingNotifyPayload, TeamMemberNotifyPayload,
    TeamMemberShuffleSuggestionNotifyPayload, TeamNotifyPayload, TeamPresetCatalogNotifyPayload,
};

use crate::send::send_frame;
use crate::state::MobileShellError;
use crate::state::{
    ActiveAgentRef, AgentInfo, AgentRef, AppState, ChatMessageEntry, ConnectionStatus,
    HostBrowseSession, LocalHostId, ProjectDiffRef, ProjectFileRef, ProjectFileState, ProjectInfo,
    ReviewRef, SessionInfo, StreamingState, ToolRequestEntry, TransientEvent,
    reduce_project_diff_response, sort_project_infos,
};

struct FrontendSeqValidator {
    expected: HashMap<(LocalHostId, StreamPath), u64>,
}

impl FrontendSeqValidator {
    fn new() -> Self {
        Self {
            expected: HashMap::new(),
        }
    }

    fn validate(
        &mut self,
        host: &LocalHostId,
        stream: &StreamPath,
        seq: u64,
        kind: FrameKind,
    ) -> Result<(), SeqMismatch> {
        let key = (host.clone(), stream.clone());
        let expected = self.expected.get(&key).copied().unwrap_or(0);
        if seq != expected {
            return Err(SeqMismatch {
                stream: stream.clone(),
                kind,
                expected,
                got: seq,
            });
        }
        self.expected.insert(key, expected + 1);
        Ok(())
    }

    fn reset_host(&mut self, host: &LocalHostId) {
        self.expected.retain(|(h, _), _| h != host);
    }
}

/// Per-`LocalHostId` `ProtocolValidator` map. Phase C HIGH 2: a single global
/// validator keyed by `StreamPath` collides when two hosts use the same
/// `/agent/...` path, and a host's reconnect replaying the same stream is
/// rejected as a duplicate. Scope by `(LocalHostId, StreamPath)` instead.
struct PerHostProtocolValidators {
    by_host: HashMap<LocalHostId, ProtocolValidator>,
}

impl PerHostProtocolValidators {
    fn new() -> Self {
        Self {
            by_host: HashMap::new(),
        }
    }

    fn validate(&mut self, host: &LocalHostId, envelope: &Envelope) -> Result<(), String> {
        let validator = self.by_host.entry(host.clone()).or_default();
        validator
            .validate_envelope(envelope)
            .map_err(|e| format!("{e}"))
    }

    fn reset_host(&mut self, host: &LocalHostId) {
        self.by_host.remove(host);
    }
}

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
    static INBOUND_PROTOCOL: RefCell<PerHostProtocolValidators> =
        RefCell::new(PerHostProtocolValidators::new());
}

pub fn reset_inbound_seq_for_host(host: &LocalHostId) {
    INBOUND_SEQ.with(|validator| validator.borrow_mut().reset_host(host));
    INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().reset_host(host));
}

pub fn dispatch_envelope(state: &AppState, host: &LocalHostId, envelope: Envelope) {
    if let Err(error) = INBOUND_SEQ.with(|validator| {
        validator
            .borrow_mut()
            .validate(host, &envelope.stream, envelope.seq, envelope.kind)
    }) {
        let message = format!(
            "mobile frontend sequence-number violation host={} stream={} kind={}: expected {}, got {}; closing dispatch for envelope",
            host, error.stream, error.kind, error.expected, error.got
        );
        log::error!("{message}");
        report_protocol_error(state, host, message);
        return;
    }
    if let Err(error) =
        INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().validate(host, &envelope))
    {
        let message = format!("protocol violation on host {host}: {error}");
        log::error!("{message}");
        report_protocol_error(state, host, message);
        return;
    }

    match envelope.kind {
        FrameKind::Welcome => {
            state.command_errors_by_host.update(|map| {
                map.remove(host);
            });
            state.connection_statuses.update(|map| {
                map.insert(host.clone(), ConnectionStatus::Connected);
            });
            clear_phase2_snapshots_for_host(state, host);

            if let Some(stream) = state.host_stream_untracked(host) {
                let host = host.clone();
                spawn_local(async move {
                    let _ = send_frame(
                        &host,
                        stream,
                        FrameKind::ListSessions,
                        &ListSessionsPayload {},
                    )
                    .await;
                });
            }

            log::info!("connected to host {}", host);
        }
        FrameKind::Reject => {
            if let Ok(payload) = envelope.parse_payload::<RejectPayload>() {
                log::error!("connection rejected on {host}: {}", payload.message);
                state.connection_statuses.update(|map| {
                    map.insert(host.clone(), ConnectionStatus::Error(payload.message));
                });
            }
        }
        FrameKind::CommandError => {
            if let Ok(payload) = envelope.parse_payload::<CommandErrorPayload>() {
                let message = format!(
                    "{} failed on {}: {}",
                    payload.operation, payload.stream, payload.message
                );
                log::error!("command error on {host}: {message}");
                state.command_errors_by_host.update(|map| {
                    map.insert(host.clone(), message);
                });
            }
        }
        FrameKind::HostSettings => {
            if let Ok(payload) = envelope.parse_payload::<HostSettingsPayload>() {
                state.host_settings_by_host.update(|map| {
                    map.insert(host.clone(), payload.settings);
                });
            }
        }
        FrameKind::BackendSetup => {
            if let Ok(payload) = envelope.parse_payload::<BackendSetupPayload>() {
                state.backend_setup_by_host.update(|map| {
                    map.insert(host.clone(), payload.backends);
                });
            }
        }
        FrameKind::SessionSchemas => {
            if let Ok(payload) = envelope.parse_payload::<SessionSchemasPayload>() {
                let mut schemas = HashMap::new();
                for schema in payload.schemas {
                    schemas.insert(schema.backend_kind(), schema);
                }
                state.session_schemas_by_host.update(|map| {
                    map.insert(host.clone(), schemas);
                });
            }
        }
        FrameKind::SessionSettings => {
            let agent_ref = resolve_agent_ref(state, host, &envelope.stream);
            if let (Some(agent_ref), Ok(payload)) = (
                agent_ref,
                envelope.parse_payload::<SessionSettingsPayload>(),
            ) {
                state.agent_session_settings.update(|map| {
                    map.insert(agent_ref, payload.values);
                });
            }
        }
        FrameKind::QueuedMessages => {
            let agent_ref = resolve_agent_ref(state, host, &envelope.stream);
            if let (Some(agent_ref), Ok(payload)) =
                (agent_ref, envelope.parse_payload::<QueuedMessagesPayload>())
            {
                state.agent_message_queue.update(|map| {
                    map.insert(agent_ref, payload.messages);
                });
            }
        }
        FrameKind::NewAgent => match envelope.parse_payload::<NewAgentPayload>() {
            Ok(payload) => {
                let agent_id = payload.agent_id.clone();
                let instance_stream = payload.instance_stream.clone();
                let origin = payload.origin;
                log::info!(
                    "mobile_apply_new_agent host={} agent_id={} instance_stream={} origin={:?}",
                    host,
                    agent_id,
                    instance_stream,
                    origin
                );
                let info = AgentInfo {
                    local_host_id: host.clone(),
                    agent_id: payload.agent_id,
                    name: payload.name.clone(),
                    origin,
                    backend_kind: payload.backend_kind,
                    workspace_roots: payload.workspace_roots,
                    project_id: payload.project_id,
                    parent_agent_id: payload.parent_agent_id,
                    custom_agent_id: payload.custom_agent_id,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    started: false,
                    fatal_error: None,
                };
                state.agents.update(|agents| {
                    agents.retain(|a| !(a.local_host_id == *host && a.agent_id == agent_id));
                    agents.push(info);
                });

                if matches!(origin, AgentOrigin::User)
                    && !has_compaction_in_progress_for_host(state, host)
                {
                    state.active_agent.set(Some(ActiveAgentRef {
                        local_host_id: host.clone(),
                        agent_id,
                    }));
                    state.viewing_chat.set(true);
                }
            }
            Err(error) => log::error!(
                "failed to parse NewAgent host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::AgentStart => match envelope.parse_payload::<AgentStartPayload>() {
            Ok(payload) => {
                log::info!(
                    "mobile_apply_agent_start host={} agent_id={} stream={}",
                    host,
                    payload.agent_id,
                    envelope.stream
                );
                state.agents.update(|agents| {
                    if let Some(agent) = agents
                        .iter_mut()
                        .find(|a| a.local_host_id == *host && a.agent_id == payload.agent_id)
                    {
                        agent.started = true;
                    } else {
                        log::error!(
                            "AgentStart referenced unknown agent host={} agent_id={} stream={}",
                            host,
                            payload.agent_id,
                            envelope.stream
                        );
                    }
                });
            }
            Err(error) => log::error!(
                "failed to parse AgentStart host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::AgentRenamed => {
            if let Ok(payload) = envelope.parse_payload::<AgentRenamedPayload>() {
                let agent_ref = AgentRef {
                    local_host_id: host.clone(),
                    agent_id: payload.agent_id.clone(),
                };
                state.agents.update(|agents| {
                    if let Some(agent) = agents
                        .iter_mut()
                        .find(|a| a.local_host_id == *host && a.agent_id == payload.agent_id)
                    {
                        agent.name = payload.name.clone();
                    }
                });
                state.streaming_text.update(|map| {
                    if let Some(streaming) = map.get_mut(&agent_ref) {
                        streaming.agent_name = payload.name;
                    }
                });
            }
        }
        FrameKind::AgentClosed => {
            if let Ok(payload) = envelope.parse_payload::<AgentClosedPayload>() {
                let agent_id = payload.agent_id;
                let agent_ref = AgentRef {
                    local_host_id: host.clone(),
                    agent_id: agent_id.clone(),
                };
                state.agents.update(|agents| {
                    agents.retain(|a| !(a.local_host_id == *host && a.agent_id == agent_id));
                });
                drop_agent_state(state, &agent_ref);

                let was_active = state.active_agent.with_untracked(|a| {
                    a.as_ref()
                        .is_some_and(|a| a.local_host_id == *host && a.agent_id == agent_id)
                });
                if was_active {
                    state.active_agent.set(None);
                    state.viewing_chat.set(false);
                }
            }
        }
        FrameKind::AgentError => {
            if let Ok(payload) = envelope.parse_payload::<AgentErrorPayload>() {
                let agent_ref = AgentRef {
                    local_host_id: host.clone(),
                    agent_id: payload.agent_id.clone(),
                };
                if payload.fatal {
                    state.agents.update(|agents| {
                        if let Some(agent) = agents
                            .iter_mut()
                            .find(|a| a.local_host_id == *host && a.agent_id == payload.agent_id)
                        {
                            agent.fatal_error = Some(payload.message.clone());
                        }
                    });
                }
                let entry = ChatMessageEntry {
                    message: protocol::ChatMessage {
                        timestamp: js_sys::Date::now() as u64,
                        sender: protocol::MessageSender::Error,
                        content: payload.message,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                    tool_requests: Vec::new(),
                };
                state.chat_messages.update(|map| {
                    map.entry(agent_ref).or_default().push(entry);
                });
            }
        }
        FrameKind::ChatEvent => dispatch_chat_event(state, host, &envelope.stream, &envelope),
        FrameKind::SessionList => match envelope.parse_payload::<SessionListPayload>() {
            Ok(payload) => {
                let count = payload.sessions.len();
                log::info!("mobile_apply_session_list host={} count={}", host, count);
                state.sessions.update(|sessions| {
                    sessions.retain(|s| s.local_host_id != *host);
                    sessions.extend(payload.sessions.into_iter().map(|summary| SessionInfo {
                        local_host_id: host.clone(),
                        summary,
                    }));
                });
            }
            Err(error) => log::error!(
                "failed to parse SessionList host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::ProjectNotify => {
            if let Ok(payload) = envelope.parse_payload::<ProjectNotifyPayload>() {
                match payload {
                    ProjectNotifyPayload::Upsert { project } => {
                        state.projects.update(|projects| {
                            if let Some(existing) = projects
                                .iter_mut()
                                .find(|e| e.local_host_id == *host && e.project.id == project.id)
                            {
                                existing.project = project;
                            } else {
                                projects.push(ProjectInfo {
                                    local_host_id: host.clone(),
                                    project,
                                });
                            }
                            sort_project_infos(projects);
                        });
                    }
                    ProjectNotifyPayload::Delete { project } => {
                        let pid = project.id.clone();
                        state.projects.update(|projects| {
                            projects.retain(|e| !(e.local_host_id == *host && e.project.id == pid));
                        });
                        state.file_tree.update(|m| {
                            m.remove(&(host.clone(), project.id.clone()));
                        });
                        state.git_status.update(|m| {
                            m.remove(&(host.clone(), project.id));
                        });
                        state.project_file_contents.update(|m| {
                            m.retain(|key, _| {
                                !(key.local_host_id == *host && key.project_id == pid)
                            });
                        });
                        state.project_diffs.update(|m| {
                            m.retain(|key, _| {
                                !(key.local_host_id == *host && key.project_id == pid)
                            });
                        });
                        state.review_summaries.update(|m| {
                            m.remove(&(host.clone(), pid));
                        });
                    }
                }
            }
        }
        FrameKind::ProjectFileList => {
            if let Some(project_id) = resolve_project_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ProjectFileListPayload>()
            {
                state.file_tree.update(|file_tree| {
                    apply_project_file_list(file_tree, host, project_id, payload);
                });
            }
        }
        FrameKind::ProjectGitStatus => {
            if let Some(project_id) = resolve_project_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ProjectGitStatusPayload>()
            {
                state.git_status.update(|git_status| {
                    git_status.insert((host.clone(), project_id), payload.roots);
                });
            }
        }
        FrameKind::ProjectFileContents => {
            if let Some(project_id) = resolve_project_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ProjectFileContentsPayload>()
            {
                let key = ProjectFileRef {
                    local_host_id: host.clone(),
                    project_id,
                    path: payload.path.clone(),
                };
                state.project_file_contents.update(|files| {
                    files.insert(key, ProjectFileState::from(payload));
                });
            }
        }
        FrameKind::ProjectGitDiff => {
            if let Some(project_id) = resolve_project_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ProjectGitDiffPayload>()
            {
                let key = ProjectDiffRef {
                    local_host_id: host.clone(),
                    project_id,
                    root: payload.root.clone(),
                    scope: payload.scope,
                    path: payload.path.clone(),
                };
                let current = state
                    .project_diffs
                    .with_untracked(|diffs| diffs.get(&key).cloned());
                if let Some(next) = reduce_project_diff_response(current.as_ref(), payload) {
                    state.project_diffs.update(|diffs| {
                        diffs.insert(key, next);
                    });
                }
            }
        }
        FrameKind::ProjectEvent => {
            if let Some(project_id) = resolve_project_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ProjectEventPayload>()
            {
                match payload {
                    ProjectEventPayload::ReviewListChanged { reviews } => {
                        state.review_summaries.update(|map| {
                            map.insert((host.clone(), project_id), reviews);
                        });
                    }
                }
            }
        }
        FrameKind::ReviewEvent => {
            if let Some(review_id) = resolve_review_id(&envelope.stream)
                && let Ok(payload) = envelope.parse_payload::<ReviewEventPayload>()
            {
                apply_review_event(state, host, review_id, payload);
            }
        }
        FrameKind::HostBrowseOpened => {
            if let Ok(payload) = envelope.parse_payload::<HostBrowseOpenedPayload>() {
                let key = (host.clone(), envelope.stream.clone());
                state.host_browses.update(|browses| {
                    let session = browses.entry(key).or_insert_with(|| HostBrowseSession {
                        local_host_id: host.clone(),
                        stream: envelope.stream.clone(),
                        opened: None,
                        entries_by_path: HashMap::new(),
                        latest_error: None,
                    });
                    session.opened = Some(payload);
                    session.latest_error = None;
                });
            }
        }
        FrameKind::HostBrowseEntries => {
            if let Ok(payload) = envelope.parse_payload::<HostBrowseEntriesPayload>() {
                let key = (host.clone(), envelope.stream.clone());
                state.host_browses.update(|browses| {
                    let session = browses.entry(key).or_insert_with(|| HostBrowseSession {
                        local_host_id: host.clone(),
                        stream: envelope.stream.clone(),
                        opened: None,
                        entries_by_path: HashMap::new(),
                        latest_error: None,
                    });
                    session
                        .entries_by_path
                        .insert(payload.path.clone(), payload);
                    session.latest_error = None;
                });
            }
        }
        FrameKind::HostBrowseError => {
            if let Ok(payload) = envelope.parse_payload::<HostBrowseErrorPayload>() {
                let key = (host.clone(), envelope.stream.clone());
                state.host_browses.update(|browses| {
                    let session = browses.entry(key).or_insert_with(|| HostBrowseSession {
                        local_host_id: host.clone(),
                        stream: envelope.stream.clone(),
                        opened: None,
                        entries_by_path: HashMap::new(),
                        latest_error: None,
                    });
                    session.latest_error = Some(payload);
                });
            }
        }
        FrameKind::CustomAgentNotify => {
            if let Ok(payload) = envelope.parse_payload::<CustomAgentNotifyPayload>() {
                state.custom_agents_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        CustomAgentNotifyPayload::Upsert { custom_agent } => {
                            inner.insert(custom_agent.id.clone(), custom_agent);
                        }
                        CustomAgentNotifyPayload::Delete { id } => {
                            inner.remove(&id);
                        }
                    }
                });
            }
        }
        FrameKind::McpServerNotify => {
            if let Ok(payload) = envelope.parse_payload::<McpServerNotifyPayload>() {
                state.mcp_servers_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        McpServerNotifyPayload::Upsert { mcp_server } => {
                            inner.insert(mcp_server.id.clone(), mcp_server);
                        }
                        McpServerNotifyPayload::Delete { id } => {
                            inner.remove(&id);
                        }
                    }
                });
            }
        }
        FrameKind::SteeringNotify => {
            if let Ok(payload) = envelope.parse_payload::<SteeringNotifyPayload>() {
                state.steering_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        SteeringNotifyPayload::Upsert { steering } => {
                            inner.insert(steering.id.clone(), steering);
                        }
                        SteeringNotifyPayload::Delete { id } => {
                            inner.remove(&id);
                        }
                    }
                });
            }
        }
        FrameKind::SkillNotify => {
            if let Ok(payload) = envelope.parse_payload::<SkillNotifyPayload>() {
                state.skills_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        SkillNotifyPayload::Upsert { skill } => {
                            inner.insert(skill.id.clone(), skill);
                        }
                        SkillNotifyPayload::Delete { id } => {
                            inner.remove(&id);
                        }
                    }
                });
            }
        }
        FrameKind::TeamNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamNotifyPayload>() {
                state.teams_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        TeamNotifyPayload::Upsert { team } => {
                            inner.insert(team.id.clone(), team);
                        }
                        TeamNotifyPayload::Delete { team } => {
                            inner.remove(&team.id);
                        }
                    }
                });
            }
        }
        FrameKind::TeamMemberNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamMemberNotifyPayload>() {
                state.team_members_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        TeamMemberNotifyPayload::Upsert { member } => {
                            inner.insert(member.id.clone(), member);
                        }
                        TeamMemberNotifyPayload::Delete { member } => {
                            inner.remove(&member.id);
                        }
                    }
                });
            }
        }
        FrameKind::TeamMemberBindingNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamMemberBindingNotifyPayload>() {
                state.team_bindings_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        TeamMemberBindingNotifyPayload::Upsert { binding } => {
                            inner.insert(binding.member_id.clone(), binding);
                        }
                        TeamMemberBindingNotifyPayload::Delete { binding } => {
                            inner.remove(&binding.member_id);
                        }
                    }
                });
            }
        }
        FrameKind::TeamCompactNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamCompactNotifyPayload>() {
                apply_team_compact_notify(state, host, payload);
            }
        }
        FrameKind::TeamPresetCatalogNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamPresetCatalogNotifyPayload>() {
                state.team_preset_catalog_by_host.update(|map| {
                    map.insert(host.clone(), payload.catalog);
                });
            }
        }
        FrameKind::TeamDraftNotify => {
            if let Ok(payload) = envelope.parse_payload::<TeamDraftNotifyPayload>() {
                state.team_drafts_by_host.update(|outer| {
                    let inner = outer.entry(host.clone()).or_default();
                    match payload {
                        TeamDraftNotifyPayload::Upsert { draft } => {
                            inner.insert(draft.id.clone(), draft);
                        }
                        TeamDraftNotifyPayload::Delete { draft_id } => {
                            inner.remove(&draft_id);
                        }
                    }
                });
            }
        }
        FrameKind::TeamMemberShuffleSuggestionNotify => {
            if let Ok(payload) =
                envelope.parse_payload::<TeamMemberShuffleSuggestionNotifyPayload>()
            {
                state.team_shuffle_suggestions_by_host.update(|outer| {
                    outer
                        .entry(host.clone())
                        .or_default()
                        .insert(payload.team_id, payload.suggestion);
                });
            }
        }
        FrameKind::AgentCompactNotify => {
            if let Ok(payload) = envelope.parse_payload::<AgentCompactNotifyPayload>() {
                apply_agent_compact_notify(state, host, payload);
            }
        }
        _ => {
            log::warn!("unhandled frame kind: {}", envelope.kind);
        }
    }
}

fn report_protocol_error(state: &AppState, host: &LocalHostId, message: String) {
    state.connection_statuses.update(|map| {
        map.insert(host.clone(), ConnectionStatus::Error(message.clone()));
    });
    state.mobile_shell_error.set(Some(MobileShellError {
        code: MobileAccessErrorCode::BrokerProtocol,
        message,
    }));
}

fn clear_phase2_snapshots_for_host(state: &AppState, host: &LocalHostId) {
    state.project_file_contents.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.project_diffs.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.review_summaries.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.reviews.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.review_errors.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.review_streams.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_compactions.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.teams_by_host.update(|m| {
        m.remove(host);
    });
    state.team_members_by_host.update(|m| {
        m.remove(host);
    });
    state.team_bindings_by_host.update(|m| {
        m.remove(host);
    });
    state.team_compactions_by_host.update(|m| {
        m.remove(host);
    });
    state.team_preset_catalog_by_host.update(|m| {
        m.remove(host);
    });
    state.team_drafts_by_host.update(|m| {
        m.remove(host);
    });
    state.team_shuffle_suggestions_by_host.update(|m| {
        m.remove(host);
    });
    state.host_browses.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
}

fn has_compaction_in_progress_for_host(state: &AppState, host: &LocalHostId) -> bool {
    state.agent_compactions.with_untracked(|map| {
        map.iter().any(|(agent_ref, payload)| {
            agent_ref.local_host_id == *host && payload.status == AgentCompactStatus::Started
        })
    })
}

fn drop_agent_state(state: &AppState, agent_ref: &AgentRef) {
    state.chat_messages.update(|m| {
        m.remove(agent_ref);
    });
    state.streaming_text.update(|m| {
        m.remove(agent_ref);
    });
    state.agent_turn_active.update(|m| {
        m.remove(agent_ref);
    });
    state.transient_events.update(|m| {
        m.remove(agent_ref);
    });
    state.task_lists.update(|m| {
        m.remove(agent_ref);
    });
    state.agent_message_queue.update(|m| {
        m.remove(agent_ref);
    });
    state.agent_session_settings.update(|m| {
        m.remove(agent_ref);
    });
    state.agent_compactions.update(|m| {
        let keep_completed = m.get(agent_ref).is_some_and(|payload| {
            payload.status == AgentCompactStatus::Completed && payload.new_agent_id.is_some()
        });
        if !keep_completed {
            m.remove(agent_ref);
        }
    });
}

fn apply_agent_compact_notify(
    state: &AppState,
    host: &LocalHostId,
    payload: AgentCompactNotifyPayload,
) {
    let old_ref = AgentRef {
        local_host_id: host.clone(),
        agent_id: payload.old_agent_id.clone(),
    };
    match payload.status {
        AgentCompactStatus::Started | AgentCompactStatus::Failed => {
            state.agent_compactions.update(|map| {
                map.insert(old_ref, payload);
            });
        }
        AgentCompactStatus::Completed => {
            state.agent_compactions.update(|map| {
                map.insert(old_ref, payload);
            });
        }
    }
}

fn apply_team_compact_notify(
    state: &AppState,
    host: &LocalHostId,
    payload: TeamCompactNotifyPayload,
) {
    for result in payload.results.iter().cloned() {
        apply_agent_compact_notify(state, host, result);
    }
    let team_id = payload.team_id.clone();
    match payload.status {
        TeamCompactStatus::Started | TeamCompactStatus::Completed | TeamCompactStatus::Failed => {
            state.team_compactions_by_host.update(|outer| {
                outer
                    .entry(host.clone())
                    .or_default()
                    .insert(team_id, payload);
            });
        }
    }
}

fn apply_review_event(
    state: &AppState,
    host: &LocalHostId,
    review_id: ReviewId,
    event: ReviewEventPayload,
) {
    let key = ReviewRef {
        local_host_id: host.clone(),
        review_id: review_id.clone(),
    };
    match event {
        ReviewEventPayload::Snapshot { review } => {
            state.review_errors.update(|errors| {
                errors.remove(&key);
            });
            state.reviews.update(|reviews| {
                reviews.insert(key, review);
            });
        }
        ReviewEventPayload::CommentUpsert { comment } => {
            state.reviews.update(|reviews| {
                if let Some(review) = reviews.get_mut(&key) {
                    if let Some(existing) = review
                        .comments
                        .iter_mut()
                        .find(|existing| existing.id == comment.id)
                    {
                        *existing = comment;
                    } else {
                        review.comments.push(comment);
                    }
                    review.updated_at_ms = js_sys::Date::now() as u64;
                }
            });
        }
        ReviewEventPayload::CommentDelete { comment_id } => {
            state.reviews.update(|reviews| {
                if let Some(review) = reviews.get_mut(&key) {
                    review.comments.retain(|comment| comment.id != comment_id);
                    review.updated_at_ms = js_sys::Date::now() as u64;
                }
            });
        }
        ReviewEventPayload::SuggestionUpsert { suggestion } => {
            state.reviews.update(|reviews| {
                if let Some(review) = reviews.get_mut(&key) {
                    if let Some(existing) = review
                        .suggestions
                        .iter_mut()
                        .find(|existing| existing.id == suggestion.id)
                    {
                        *existing = suggestion;
                    } else {
                        review.suggestions.push(suggestion);
                    }
                    review.updated_at_ms = js_sys::Date::now() as u64;
                }
            });
        }
        ReviewEventPayload::AiReviewerChanged { state: reviewer } => {
            state.reviews.update(|reviews| {
                if let Some(review) = reviews.get_mut(&key) {
                    review.ai_reviewer = reviewer;
                    review.updated_at_ms = js_sys::Date::now() as u64;
                }
            });
        }
        ReviewEventPayload::StatusChanged { status } => {
            state.reviews.update(|reviews| {
                if let Some(review) = reviews.get_mut(&key) {
                    review.status = status;
                    review.updated_at_ms = js_sys::Date::now() as u64;
                }
            });
        }
        ReviewEventPayload::Error { error } => {
            state.review_errors.update(|errors| {
                errors.insert(key, error);
            });
        }
    }
}

fn apply_project_file_list(
    file_tree: &mut HashMap<(LocalHostId, ProjectId), Vec<protocol::ProjectRootListing>>,
    host: &LocalHostId,
    project_id: ProjectId,
    payload: ProjectFileListPayload,
) {
    let key = (host.clone(), project_id);
    let existing_roots = file_tree.entry(key).or_default();
    for incoming_root in payload.roots {
        let root_index = existing_roots
            .iter()
            .position(|existing| existing.root == incoming_root.root)
            .unwrap_or_else(|| {
                existing_roots.push(protocol::ProjectRootListing {
                    root: incoming_root.root.clone(),
                    entries: Vec::new(),
                });
                existing_roots.len() - 1
            });
        let existing_root = &mut existing_roots[root_index];

        let mut existing_paths: HashSet<String> = existing_root
            .entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect();
        let mut removed_paths = HashSet::new();
        for entry in incoming_root.entries {
            match entry.op {
                protocol::FileEntryOp::Add => {
                    if existing_paths.insert(entry.relative_path.clone()) {
                        removed_paths.remove(&entry.relative_path);
                        existing_root.entries.push(entry);
                    }
                }
                protocol::FileEntryOp::Remove => {
                    existing_paths.remove(&entry.relative_path);
                    removed_paths.insert(entry.relative_path);
                }
            }
        }
        if !removed_paths.is_empty() {
            existing_root
                .entries
                .retain(|existing| !removed_paths.contains(&existing.relative_path));
        }
        existing_root
            .entries
            .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    }
}

fn resolve_project_id(stream: &StreamPath) -> Option<ProjectId> {
    let suffix = stream.0.strip_prefix("/project/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ProjectId(suffix.to_string()))
}

fn resolve_review_id(stream: &StreamPath) -> Option<ReviewId> {
    let suffix = stream.0.strip_prefix("/review/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ReviewId(suffix.to_string()))
}

fn resolve_agent_ref(
    state: &AppState,
    host: &LocalHostId,
    stream: &StreamPath,
) -> Option<AgentRef> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| agent.local_host_id == *host && agent.instance_stream == *stream)
            .map(|agent| agent.agent_ref())
    })
}

fn resolve_agent_id(state: &AppState, host: &LocalHostId, stream: &StreamPath) -> Option<AgentId> {
    resolve_agent_ref(state, host, stream).map(|r| r.agent_id)
}

fn dispatch_chat_event(
    state: &AppState,
    host: &LocalHostId,
    stream: &StreamPath,
    envelope: &Envelope,
) {
    let Some(agent_id) = resolve_agent_id(state, host, stream) else {
        let known_streams = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .filter(|agent| agent.local_host_id == *host)
                .map(|agent| agent.instance_stream.0.clone())
                .collect::<Vec<_>>()
        });
        log::error!(
            "chat_event on unknown stream host={} stream={} known_streams={:?}",
            host,
            stream,
            known_streams
        );
        return;
    };
    let agent_ref = AgentRef {
        local_host_id: host.clone(),
        agent_id,
    };

    let event = match envelope.parse_payload::<ChatEvent>() {
        Ok(event) => event,
        Err(error) => {
            log::error!(
                "failed to parse chat_event payload host={} stream={} seq={}: {}",
                host,
                stream,
                envelope.seq,
                error
            );
            return;
        }
    };
    log::info!(
        "mobile_apply_chat_event host={} agent_id={} stream={} event={}",
        host,
        agent_ref.agent_id,
        stream,
        chat_event_label(&event)
    );

    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            if typing {
                state.transient_events.update(|events| {
                    events.remove(&agent_ref);
                });
            }
            state.agent_turn_active.update(|map| {
                if typing {
                    map.insert(agent_ref.clone(), true);
                } else {
                    map.remove(&agent_ref);
                }
            });
        }
        ChatEvent::MessageAdded(message) => {
            let entry = ChatMessageEntry {
                message,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(|messages| {
                messages.entry(agent_ref.clone()).or_default().push(entry);
            });
        }
        ChatEvent::StreamStart(data) => {
            state.transient_events.update(|events| {
                events.remove(&agent_ref);
            });
            let streaming = StreamingState {
                agent_name: data.agent,
                model: data.model,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            };
            state.streaming_text.update(|map| {
                map.insert(agent_ref.clone(), streaming);
            });
        }
        ChatEvent::StreamDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            if let Some(streaming) = streaming {
                streaming.text.update(|text| text.push_str(&data.text));
            } else {
                log::error!(
                    "StreamDelta without StreamStart host={} agent_id={} stream={}",
                    host,
                    agent_ref.agent_id,
                    stream
                );
            }
        }
        ChatEvent::StreamReasoningDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            if let Some(streaming) = streaming {
                streaming
                    .reasoning
                    .update(|reasoning| reasoning.push_str(&data.text));
            } else {
                log::error!(
                    "StreamReasoningDelta without StreamStart host={} agent_id={} stream={}",
                    host,
                    agent_ref.agent_id,
                    stream
                );
            }
        }
        ChatEvent::StreamEnd(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            let tool_requests = streaming
                .as_ref()
                .map(|s| s.tool_requests.get_untracked())
                .unwrap_or_default();
            state.streaming_text.update(|map| {
                map.remove(&agent_ref);
            });
            let has_renderable_content = !data.message.content.trim().is_empty()
                || data
                    .message
                    .reasoning
                    .as_ref()
                    .is_some_and(|r| !r.text.trim().is_empty())
                || !data.message.tool_calls.is_empty()
                || data
                    .message
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty())
                || !tool_requests.is_empty();
            if !has_renderable_content {
                return;
            }
            let entry = ChatMessageEntry {
                message: data.message,
                tool_requests,
            };
            state.chat_messages.update(|messages| {
                messages.entry(agent_ref.clone()).or_default().push(entry);
            });
        }
        ChatEvent::ToolRequest(request) => {
            let tool_entry = ToolRequestEntry {
                request,
                result: None,
            };
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            if let Some(streaming) = streaming {
                streaming
                    .tool_requests
                    .update(|tools| tools.push(tool_entry));
                return;
            }
            state.chat_messages.update(|messages| {
                if let Some(agent_messages) = messages.get_mut(&agent_ref)
                    && let Some(last) = agent_messages.last_mut()
                {
                    last.tool_requests.push(tool_entry);
                }
            });
        }
        ChatEvent::ToolExecutionCompleted(data) => {
            let call_id = data.tool_call_id.clone();
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            if let Some(streaming) = streaming {
                let mut matched = false;
                streaming.tool_requests.update(|tools| {
                    if let Some(tool) = tools
                        .iter_mut()
                        .find(|tool| tool.request.tool_call_id == call_id)
                    {
                        tool.result = Some(data.clone());
                        matched = true;
                    }
                });
                if matched {
                    return;
                }
            }
            state.chat_messages.update(|messages| {
                if let Some(agent_messages) = messages.get_mut(&agent_ref) {
                    for message in agent_messages.iter_mut().rev() {
                        if let Some(tool) = message
                            .tool_requests
                            .iter_mut()
                            .find(|tool| tool.request.tool_call_id == call_id)
                        {
                            tool.result = Some(data);
                            return;
                        }
                    }
                }
            });
        }
        ChatEvent::TaskUpdate(task_list) => {
            state.task_lists.update(|task_lists| {
                task_lists.insert(agent_ref.clone(), task_list);
            });
        }
        ChatEvent::OperationCancelled(data) => {
            state.streaming_text.update(|map| {
                map.remove(&agent_ref);
            });
            state.transient_events.update(|events| {
                events
                    .entry(agent_ref)
                    .or_default()
                    .push(TransientEvent::OperationCancelled {
                        message: data.message,
                    });
            });
        }
        ChatEvent::RetryAttempt(data) => {
            state.transient_events.update(|events| {
                events
                    .entry(agent_ref)
                    .or_default()
                    .push(TransientEvent::RetryAttempt {
                        attempt: data.attempt,
                        max_retries: data.max_retries,
                        error: data.error,
                        backoff_ms: data.backoff_ms,
                    });
            });
        }
    }
}

fn chat_event_label(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::TypingStatusChanged(_) => "TypingStatusChanged",
        ChatEvent::MessageAdded(_) => "MessageAdded",
        ChatEvent::StreamStart(_) => "StreamStart",
        ChatEvent::StreamDelta(_) => "StreamDelta",
        ChatEvent::StreamReasoningDelta(_) => "StreamReasoningDelta",
        ChatEvent::StreamEnd(_) => "StreamEnd",
        ChatEvent::ToolRequest(_) => "ToolRequest",
        ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted",
        ChatEvent::TaskUpdate(_) => "TaskUpdate",
        ChatEvent::OperationCancelled(_) => "OperationCancelled",
        ChatEvent::RetryAttempt(_) => "RetryAttempt",
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::prelude::{GetUntracked, WithUntracked};
    use protocol::{
        AgentId, DiffContextMode, Envelope, FrameKind, HostAbsPath, HostBrowseOpenedPayload,
        HostPlatform, ProjectDiffScope, ProjectFileContentsPayload, ProjectGitDiffPayload,
        ProjectId, ProjectPath, ProjectRootPath, ReviewAiReviewerState, ReviewAiReviewerStatus,
        ReviewComment, ReviewCommentId, ReviewCommentSource, ReviewDiffSelection,
        ReviewEventPayload, ReviewId, ReviewStatus, StreamPath, TeamCompactNotifyPayload,
        TeamCompactStatus, TeamId, TeamMemberId,
    };
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn host(id: &str) -> LocalHostId {
        let host = LocalHostId(id.to_owned());
        reset_inbound_seq_for_host(&host);
        host
    }

    fn envelope<T: serde::Serialize>(
        stream: &str,
        kind: FrameKind,
        seq: u64,
        payload: &T,
    ) -> Envelope {
        Envelope::from_payload(StreamPath(stream.to_owned()), kind, seq, payload).expect("envelope")
    }

    #[wasm_bindgen_test]
    fn protocol_errors_surface_when_sequence_numbers_gap() {
        let state = AppState::new();
        let host = host("mobile-seq-gap");

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-seq-gap",
                FrameKind::ListSessions,
                1,
                &ListSessionsPayload {},
            ),
        );

        assert!(matches!(
            state.connection_statuses.get_untracked().get(&host),
            Some(ConnectionStatus::Error(message))
                if message.contains("sequence-number violation")
                    && message.contains("expected 0, got 1")
        ));
        let error = state
            .mobile_shell_error
            .get_untracked()
            .expect("shell error");
        assert_eq!(error.code, MobileAccessErrorCode::BrokerProtocol);
        assert!(error.message.contains("closing dispatch"));
    }

    #[wasm_bindgen_test]
    fn dispatch_project_file_and_diff_updates_host_keyed_state() {
        let state = AppState::new();
        let host = host("mobile-project-foundation");
        let project_id = ProjectId("project-a".to_owned());
        let root = ProjectRootPath("/repo".to_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "src/lib.rs".to_owned(),
        };

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/project/project-a",
                FrameKind::ProjectFileContents,
                0,
                &ProjectFileContentsPayload {
                    path: path.clone(),
                    contents: Some("fn main() {}".to_owned()),
                    is_binary: false,
                },
            ),
        );
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/project/project-a",
                FrameKind::ProjectGitDiff,
                1,
                &ProjectGitDiffPayload {
                    root: root.clone(),
                    scope: ProjectDiffScope::Uncommitted,
                    path: Some("src/lib.rs".to_owned()),
                    context_mode: DiffContextMode::Hunks,
                    files: Vec::new(),
                },
            ),
        );

        assert_eq!(state.project_file_contents.get_untracked().len(), 1);
        let diff_key = ProjectDiffRef {
            local_host_id: host,
            project_id,
            root,
            scope: ProjectDiffScope::Uncommitted,
            path: Some("src/lib.rs".to_owned()),
        };
        let diff = state
            .project_diffs
            .get_untracked()
            .get(&diff_key)
            .cloned()
            .expect("diff state");
        assert!(!diff.pending);
        assert_eq!(diff.context_mode, DiffContextMode::Hunks);
    }

    #[wasm_bindgen_test]
    fn dispatch_review_snapshot_and_comment_delta() {
        let state = AppState::new();
        let host = host("mobile-review-foundation");
        let review_id = ReviewId("review-a".to_owned());
        let review = protocol::Review {
            id: review_id.clone(),
            project_id: ProjectId("project-a".to_owned()),
            origin_agent_id: AgentId("agent-a".to_owned()),
            origin_session_id: protocol::SessionId("session-a".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: Vec::new(),
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/review/review-a",
                FrameKind::ReviewEvent,
                0,
                &ReviewEventPayload::Snapshot { review },
            ),
        );
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/review/review-a",
                FrameKind::ReviewEvent,
                1,
                &ReviewEventPayload::CommentUpsert {
                    comment: ReviewComment {
                        id: ReviewCommentId("comment-a".to_owned()),
                        location: protocol::ReviewLocation {
                            root: ProjectRootPath("/repo".to_owned()),
                            relative_path: "src/lib.rs".to_owned(),
                            anchor: protocol::ReviewAnchor::File,
                        },
                        body: "Looks good".to_owned(),
                        source: ReviewCommentSource::User,
                        created_at_ms: 2,
                        updated_at_ms: 2,
                    },
                },
            ),
        );

        let key = ReviewRef {
            local_host_id: host,
            review_id,
        };
        let stored = state
            .reviews
            .get_untracked()
            .get(&key)
            .cloned()
            .expect("review stored");
        assert_eq!(stored.comments.len(), 1);
        assert_eq!(stored.comments[0].body, "Looks good");
    }

    #[wasm_bindgen_test]
    fn compaction_completion_preserves_active_chat_and_tap_target() {
        let state = AppState::new();
        let host = host("mobile-compaction-navigation");
        let old_agent_id = AgentId("old-agent".to_owned());
        let new_agent_id = AgentId("new-agent".to_owned());

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/h",
                FrameKind::NewAgent,
                0,
                &protocol::NewAgentPayload {
                    agent_id: old_agent_id.clone(),
                    name: "Old".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    custom_agent_id: None,
                    team_id: None,
                    team_member_id: None,
                    project_id: None,
                    parent_agent_id: None,
                    created_at_ms: 1,
                    instance_stream: StreamPath("/agent/old-agent/inst".to_owned()),
                },
            ),
        );
        assert_eq!(
            state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                local_host_id: host.clone(),
                agent_id: old_agent_id.clone(),
            })
        );

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/agent/old-agent/inst",
                FrameKind::AgentCompactNotify,
                0,
                &AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Started,
                    old_agent_id: old_agent_id.clone(),
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: None,
                },
            ),
        );
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/h",
                FrameKind::NewAgent,
                1,
                &protocol::NewAgentPayload {
                    agent_id: new_agent_id.clone(),
                    name: "Replacement".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    custom_agent_id: None,
                    team_id: None,
                    team_member_id: None,
                    project_id: None,
                    parent_agent_id: None,
                    created_at_ms: 2,
                    instance_stream: StreamPath("/agent/new-agent/inst".to_owned()),
                },
            ),
        );
        assert_eq!(
            state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                local_host_id: host.clone(),
                agent_id: old_agent_id.clone(),
            }),
            "replacement NewAgent during compaction should not auto-open"
        );

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/agent/old-agent/inst",
                FrameKind::AgentCompactNotify,
                1,
                &AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Completed,
                    old_agent_id: old_agent_id.clone(),
                    old_session_id: None,
                    new_agent_id: Some(new_agent_id.clone()),
                    new_session_id: None,
                    summary_preview: Some("remembered".to_owned()),
                    message: None,
                },
            ),
        );

        assert_eq!(
            state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                local_host_id: host.clone(),
                agent_id: old_agent_id.clone(),
            }),
            "compaction completion should not auto-switch to the replacement"
        );
        let old_ref = AgentRef {
            local_host_id: host.clone(),
            agent_id: old_agent_id.clone(),
        };
        assert_eq!(
            state
                .agent_compactions
                .get_untracked()
                .get(&old_ref)
                .and_then(|payload| payload.new_agent_id.as_ref()),
            Some(&new_agent_id)
        );

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/h",
                FrameKind::AgentClosed,
                2,
                &protocol::AgentClosedPayload {
                    agent_id: old_agent_id,
                },
            ),
        );

        assert_eq!(state.active_agent.get_untracked(), None);
        assert!(
            state.agent_compactions.with_untracked(|map| map
                .get(&old_ref)
                .is_some_and(|payload| payload.new_agent_id.as_ref() == Some(&new_agent_id))),
            "closed compacted agent should retain completed payload for a toast/tap target"
        );
    }

    #[wasm_bindgen_test]
    fn dispatch_team_browse_and_compaction_foundation() {
        let state = AppState::new();
        let host = host("mobile-team-browse-foundation");

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/h",
                FrameKind::TeamCompactNotify,
                0,
                &TeamCompactNotifyPayload {
                    status: TeamCompactStatus::Started,
                    team_id: TeamId("team-a".to_owned()),
                    member_ids: vec![TeamMemberId("member-a".to_owned())],
                    agent_ids: vec![AgentId("agent-a".to_owned())],
                    results: vec![AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Started,
                        old_agent_id: AgentId("agent-a".to_owned()),
                        old_session_id: None,
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    }],
                    message: None,
                },
            ),
        );
        assert!(state.team_compactions_by_host.with_untracked(|map| {
            map.get(&host)
                .is_some_and(|teams| teams.contains_key(&TeamId("team-a".to_owned())))
        }));

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/browse/browse-a",
                FrameKind::HostBrowseOpened,
                0,
                &HostBrowseOpenedPayload {
                    home: HostAbsPath("/Users/mike".to_owned()),
                    root: HostAbsPath("/".to_owned()),
                    separator: '/',
                    platform: HostPlatform::Macos,
                },
            ),
        );
        assert!(state.host_browses.with_untracked(|browses| {
            browses
                .get(&(host, StreamPath("/browse/browse-a".to_owned())))
                .and_then(|session| session.opened.as_ref())
                .is_some()
        }));
    }
}
