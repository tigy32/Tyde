use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};

use protocol::MobileAccessErrorCode;
use protocol::types::{AgentCompactNotifyPayload, AgentCompactStatus};
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentClosedPayload, AgentErrorPayload, AgentId,
    AgentOrigin, AgentRenamedPayload, AgentStartPayload, BackendSetupPayload,
    BrowseBootstrapListing, BrowseBootstrapPayload, ChatEvent, ClientErrorCode,
    CommandErrorPayload, CustomAgentNotifyPayload, Envelope, FrameKind, HostBootstrapPayload,
    HostBrowseEntriesPayload, HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload,
    McpServerNotifyPayload, NewAgentPayload, ProjectBootstrapPayload, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload, ProtocolValidator,
    QueuedMessagesPayload, RejectPayload, ReviewBootstrapPayload, ReviewEventPayload, ReviewId,
    SeqMismatch, SessionListPayload, SessionSchemasPayload, SessionSettingsPayload,
    SkillNotifyPayload, SteeringNotifyPayload, StreamPath, TeamCompactNotifyPayload,
    TeamCompactStatus, TeamDraftNotifyPayload, TeamMemberBindingNotifyPayload,
    TeamMemberNotifyPayload, TeamMemberShuffleSuggestionNotifyPayload, TeamNotifyPayload,
    TeamPresetCatalogNotifyPayload,
};

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

/// Test helper: prime the inbound validators for `host` so subsequent
/// dispatch calls behave as if the server had already delivered a
/// `Welcome` (seq 0) + `HostBootstrap` (seq 1) pair on the
/// `/host/<host>` stream.
///
/// After priming, the `ProtocolValidator` considers the host stream to
/// have observed a bootstrap, but the `FrontendSeqValidator` has been
/// rewound so tests can dispatch their first envelope at seq `0`
/// without a seq-mismatch error.
#[allow(dead_code)]
pub fn prime_host_for_tests(state: &AppState, host: &LocalHostId) {
    use protocol::{
        BackendSetupPayload as BootstrapBackendSetup, HostBootstrapPayload as BootstrapHostPayload,
        HostSettings as BootstrapHostSettings, MobileAccessStatePayload as BootstrapMobileAccess,
        MobileBrokerStatus as BootstrapBrokerStatus, MobilePairingState as BootstrapPairingState,
        PROTOCOL_VERSION, TYDE_VERSION, TeamPresetCatalog as BootstrapTeamPresetCatalog,
        WelcomePayload as BootstrapWelcome,
    };

    reset_inbound_seq_for_host(host);

    let host_stream = StreamPath(format!("/host/{}", host.0));
    let welcome = BootstrapWelcome {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        release_version: None,
    };
    let bootstrap = BootstrapHostPayload {
        settings: BootstrapHostSettings {
            enabled_backends: Vec::new(),
            default_backend: None,
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
        },
        mobile_access: BootstrapMobileAccess {
            broker_status: BootstrapBrokerStatus::Disabled,
            pairing: BootstrapPairingState::Idle,
            paired_devices: Vec::new(),
        },
        backend_setup: BootstrapBackendSetup {
            backends: Vec::new(),
        },
        session_schemas: Vec::new(),
        sessions: Vec::new(),
        projects: Vec::new(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        steering: Vec::new(),
        custom_agents: Vec::new(),
        team_preset_catalog: BootstrapTeamPresetCatalog {
            role_presets: Vec::new(),
            personality_traits: Vec::new(),
            personality_presets: Vec::new(),
            team_templates: Vec::new(),
        },
        team_drafts: Vec::new(),
        teams: Vec::new(),
        team_members: Vec::new(),
        team_member_bindings: Vec::new(),
        agents: Vec::new(),
        workflow_summaries: Vec::new(),
        workflow_diagnostics: Vec::new(),
        workflow_runs: Vec::new(),
        workflow_locations: Vec::new(),
    };

    let welcome_env = Envelope::from_payload(host_stream.clone(), FrameKind::Welcome, 0, &welcome)
        .expect("synthetic Welcome");
    dispatch_envelope(state, host, welcome_env);
    let bootstrap_env =
        Envelope::from_payload(host_stream, FrameKind::HostBootstrap, 1, &bootstrap)
            .expect("synthetic HostBootstrap");
    dispatch_envelope(state, host, bootstrap_env);

    // Rewind only the FrontendSeqValidator. The ProtocolValidator keeps
    // the saw_welcome/saw_bootstrap state from the synthetic frames.
    INBOUND_SEQ.with(|validator| validator.borrow_mut().reset_host(host));
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
            // Sessions/projects/teams etc. arrive via HostBootstrap (seq 1
            // on the host stream).  We do not pre-clear anything here because
            // apply_host_bootstrap replaces each collection atomically, so
            // data stays visible until the moment it is refreshed rather than
            // flashing blank between Welcome and HostBootstrap.
            log::info!("connected to host {}", host);
        }
        FrameKind::HostBootstrap => match envelope.parse_payload::<HostBootstrapPayload>() {
            Ok(payload) => apply_host_bootstrap(state, host, payload),
            Err(error) => log::error!(
                "failed to parse HostBootstrap host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::AgentBootstrap => match envelope.parse_payload::<AgentBootstrapPayload>() {
            Ok(payload) => apply_agent_bootstrap(state, host, &envelope.stream, payload),
            Err(error) => log::error!(
                "failed to parse AgentBootstrap host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::ProjectBootstrap => {
            if let Some(project_id) = resolve_project_id(&envelope.stream) {
                match envelope.parse_payload::<ProjectBootstrapPayload>() {
                    Ok(payload) => apply_project_bootstrap(state, host, project_id, payload),
                    Err(error) => log::error!(
                        "failed to parse ProjectBootstrap host={} stream={} seq={}: {}",
                        host,
                        envelope.stream,
                        envelope.seq,
                        error
                    ),
                }
            }
        }
        FrameKind::ReviewBootstrap => {
            if let Some(review_id) = resolve_review_id(&envelope.stream) {
                match envelope.parse_payload::<ReviewBootstrapPayload>() {
                    Ok(payload) => apply_review_bootstrap(state, host, review_id, payload),
                    Err(error) => log::error!(
                        "failed to parse ReviewBootstrap host={} stream={} seq={}: {}",
                        host,
                        envelope.stream,
                        envelope.seq,
                        error
                    ),
                }
            }
        }
        FrameKind::BrowseBootstrap => match envelope.parse_payload::<BrowseBootstrapPayload>() {
            Ok(payload) => apply_browse_bootstrap(state, host, &envelope.stream, payload),
            Err(error) => log::error!(
                "failed to parse BrowseBootstrap host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::TerminalBootstrap => {
            // mobile does not yet model terminals — log and ignore.
            log::info!(
                "ignoring TerminalBootstrap on mobile host={} stream={}",
                host,
                envelope.stream
            );
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
                let started = payload.session_id.is_some();
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
                    session_id: payload.session_id,
                    custom_agent_id: payload.custom_agent_id,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    started,
                    fatal_error: None,
                };
                state.agents.update(|agents| {
                    agents.retain(|a| !(a.local_host_id == *host && a.agent_id == agent_id));
                    agents.push(info);
                });

                // User and SideQuestion (BTW) agents auto-open into the chat
                // view — a side question is something the user just asked for.
                if matches!(origin, AgentOrigin::User | AgentOrigin::SideQuestion)
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
                        if let Some(session_id) = payload.session_id {
                            agent.session_id = Some(session_id);
                        }
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
                        message_id: None,
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
        FrameKind::WorkflowNotify => {
            let _ = envelope.parse_payload::<protocol::WorkflowNotifyPayload>();
        }
        FrameKind::WorkflowRunNotify => {
            let _ = envelope.parse_payload::<protocol::WorkflowRunNotifyPayload>();
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
    // Report the seq/protocol violation back to the host so the server logs
    // it. The frame that triggered this already parsed cleanly, so there is no
    // raw offending line to forward — the structured message carries the
    // detail. Sent on the host stream via the shared outbound seq counter, so
    // it cannot itself re-enter inbound validation and loop.
    emit_client_protocol_error(state, host, message.clone());
    state.mobile_shell_error.set(Some(MobileShellError {
        code: MobileAccessErrorCode::BrokerProtocol,
        message,
    }));
}

/// Emits a `ClientError { code: ProtocolValidation }` frame to the host on its
/// stream so seq/protocol-validation failures are visible server-side. Reuses
/// `send_frame` (and thus the existing per-(host, stream) outgoing seq
/// counter); no parallel send path. If no host stream is allocated yet, or the
/// send fails, the error is logged locally and not retried.
fn emit_client_protocol_error(state: &AppState, host: &LocalHostId, message: String) {
    let Some(stream) = state.host_stream_untracked(host) else {
        log::error!("cannot report client protocol error to {host}: no host stream allocated yet");
        return;
    };
    let host = host.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let payload = protocol::ClientErrorPayload {
            code: ClientErrorCode::ProtocolValidation,
            message,
            raw_context: None,
        };
        if let Err(error) =
            crate::send::send_frame(&host, stream, FrameKind::ClientError, &payload).await
        {
            log::error!("failed to report client protocol error to {host}: {error}");
        }
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
    state.agent_load_requests.update(|m| {
        m.remove(agent_ref);
    });
    state.agent_loaded.update(|m| {
        m.remove(agent_ref);
    });
    state.chat_messages.update(|m| {
        m.remove(agent_ref);
    });
    state.chat_message_index.update(|m| {
        m.remove(agent_ref);
    });
    state.forget_history_window(agent_ref);
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
        ReviewEventPayload::Cleared { review } => {
            // Server cleared the review (after Submit, explicit ClearComments,
            // or a clean working tree). Replace the local projection with the
            // included review and drop any in-flight error gate for this id.
            state.review_errors.update(|errors| {
                errors.remove(&key);
            });
            state.reviews.update(|reviews| {
                reviews.insert(key, review);
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

    apply_chat_event(state, &agent_ref, event);
}

/// Apply an already-parsed `ChatEvent` for a known `agent_ref`.
///
/// Split out from `dispatch_chat_event` so an `AgentBootstrap` (or any
/// future code path that already holds a parsed event) can replay inner
/// events through the same reducer without re-encoding them through an
/// `Envelope`.
pub fn apply_chat_event(state: &AppState, agent_ref: &AgentRef, event: ChatEvent) {
    let agent_ref = agent_ref.clone();
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
            // A new turn starting means the restore/replay phase is over, so
            // freeze the history floor: rows from here on are genuinely new
            // conversation and should accumulate visibly rather than be
            // swallowed by the windowing tail-tracking.
            if typing {
                state.history_settling.update(|set| {
                    set.remove(&agent_ref);
                });
            }
        }
        ChatEvent::MessageAdded(message) => {
            let entry = ChatMessageEntry {
                message,
                tool_requests: Vec::new(),
            };
            state.push_chat_message_entry(&agent_ref, entry);
        }
        ChatEvent::MessageMetadataUpdated(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=message_metadata_updated message_id={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                data.message_id
            );
            state.apply_chat_message_metadata(&agent_ref, data);
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
                    "StreamDelta without StreamStart host={} agent_id={}",
                    agent_ref.local_host_id,
                    agent_ref.agent_id,
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
                    "StreamReasoningDelta without StreamStart host={} agent_id={}",
                    agent_ref.local_host_id,
                    agent_ref.agent_id,
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
            state.push_chat_message_entry(&agent_ref, entry);
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
        // Live tool progress is not rendered on mobile (v1).
        ChatEvent::ToolProgress(_) => {}
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

// ── Bootstrap apply helpers ──────────────────────────────────────────────
//
// HostBootstrap (seq 1 on the host stream after Welcome at seq 0) carries
// a full snapshot of host-scoped state. The apply helpers below replace
// each host-keyed slice in `AppState` with the snapshot. Side effects that
// depend on user intent (e.g. auto-opening a chat tab) stay in the
// per-event arms.

fn apply_host_bootstrap(state: &AppState, host: &LocalHostId, payload: HostBootstrapPayload) {
    log::info!(
        "dispatch host_bootstrap host={} sessions={} projects={} agents={} teams={}",
        host,
        payload.sessions.len(),
        payload.projects.len(),
        payload.agents.len(),
        payload.teams.len(),
    );

    state.host_settings_by_host.update(|map| {
        map.insert(host.clone(), payload.settings);
    });
    // mobile-frontend has no mobile_access state of its own (it IS the
    // mobile client) — the field is intentionally unused.
    let _ = payload.mobile_access;
    state.backend_setup_by_host.update(|map| {
        map.insert(host.clone(), payload.backend_setup.backends);
    });
    state.session_schemas_by_host.update(|map| {
        let mut schemas = HashMap::new();
        for schema in payload.session_schemas {
            schemas.insert(schema.backend_kind(), schema);
        }
        map.insert(host.clone(), schemas);
    });
    state.sessions.update(|sessions| {
        sessions.retain(|s| s.local_host_id != *host);
        sessions.extend(payload.sessions.into_iter().map(|summary| SessionInfo {
            local_host_id: host.clone(),
            summary,
        }));
    });
    state.projects.update(|projects| {
        projects.retain(|p| p.local_host_id != *host);
        projects.extend(payload.projects.into_iter().map(|project| ProjectInfo {
            local_host_id: host.clone(),
            project,
        }));
        sort_project_infos(projects);
    });
    state.agent_load_requests.update(|loads| {
        loads.retain(|agent_ref| agent_ref.local_host_id != *host);
    });
    // A fresh host snapshot re-arms lazy loading, so drop the "bootstrap
    // arrived" marker too; reopening a chat after reconnect shows a spinner
    // until its transcript is re-fetched.
    state.agent_loaded.update(|loaded| {
        loaded.retain(|agent_ref| agent_ref.local_host_id != *host);
    });
    state.mcp_servers_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for mcp_server in payload.mcp_servers {
            inner.insert(mcp_server.id.clone(), mcp_server);
        }
    });
    state.skills_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for skill in payload.skills {
            inner.insert(skill.id.clone(), skill);
        }
    });
    state.steering_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for steering in payload.steering {
            inner.insert(steering.id.clone(), steering);
        }
    });
    state.custom_agents_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for custom_agent in payload.custom_agents {
            inner.insert(custom_agent.id.clone(), custom_agent);
        }
    });
    state.team_preset_catalog_by_host.update(|map| {
        map.insert(host.clone(), payload.team_preset_catalog);
    });
    state.team_drafts_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for draft in payload.team_drafts {
            inner.insert(draft.id.clone(), draft);
        }
    });
    state.teams_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for team in payload.teams {
            inner.insert(team.id.clone(), team);
        }
    });
    state.team_members_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for member in payload.team_members {
            inner.insert(member.id.clone(), member);
        }
    });
    state.team_bindings_by_host.update(|outer| {
        let inner = outer.entry(host.clone()).or_default();
        inner.clear();
        for binding in payload.team_member_bindings {
            inner.insert(binding.member_id.clone(), binding);
        }
    });

    let snapshot_ids: HashSet<AgentId> =
        payload.agents.iter().map(|p| p.agent_id.clone()).collect();
    // Prune the history window for agents on this host the snapshot no longer
    // knows about, so a dropped agent doesn't leave an orphaned floor behind.
    let dropped_refs: Vec<AgentRef> = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .filter(|a| a.local_host_id == *host && !snapshot_ids.contains(&a.agent_id))
            .map(|a| a.agent_ref())
            .collect()
    });
    for dropped in &dropped_refs {
        state.forget_history_window(dropped);
    }
    state.agents.update(|agents| {
        agents.retain(|a| a.local_host_id != *host || snapshot_ids.contains(&a.agent_id));
        for payload in payload.agents {
            let mut info = agent_info_from_payload(host, payload);
            if let Some(existing) = agents
                .iter_mut()
                .find(|a| a.local_host_id == *host && a.agent_id == info.agent_id)
            {
                // `NewAgentPayload` may omit `session_id` before backend
                // startup completes. Preserve a more complete live event
                // stream view so a bootstrap re-application doesn't reset
                // an already-started agent to `started: false`.
                info.started = info.started || existing.started;
                info.fatal_error = existing.fatal_error.clone();
                if info.session_id.is_none() {
                    info.session_id = existing.session_id.clone();
                }
                *existing = info;
            } else {
                agents.push(info);
            }
        }
    });
}

fn agent_info_from_payload(host: &LocalHostId, payload: NewAgentPayload) -> AgentInfo {
    let started = payload.session_id.is_some();
    AgentInfo {
        local_host_id: host.clone(),
        agent_id: payload.agent_id,
        name: payload.name,
        origin: payload.origin,
        backend_kind: payload.backend_kind,
        workspace_roots: payload.workspace_roots,
        project_id: payload.project_id,
        parent_agent_id: payload.parent_agent_id,
        session_id: payload.session_id,
        custom_agent_id: payload.custom_agent_id,
        created_at_ms: payload.created_at_ms,
        instance_stream: payload.instance_stream,
        started,
        fatal_error: None,
    }
}

fn apply_agent_bootstrap(
    state: &AppState,
    host: &LocalHostId,
    stream: &StreamPath,
    payload: AgentBootstrapPayload,
) {
    let Some(agent_ref) = resolve_agent_ref(state, host, stream) else {
        log::warn!("agent_bootstrap on unknown stream host={host} stream={stream}");
        return;
    };
    log::info!(
        "dispatch agent_bootstrap host={} stream={} agent_id={} events={}",
        host,
        stream,
        agent_ref.agent_id,
        payload.events.len()
    );
    state.agent_load_requests.update(|m| {
        m.insert(agent_ref.clone());
    });
    state.agent_loaded.update(|m| {
        m.insert(agent_ref.clone());
    });
    // Replace prior per-agent chat/stream/queue/task state so the bootstrap
    // snapshot is authoritative.
    state.chat_messages.update(|m| {
        m.remove(&agent_ref);
    });
    state.chat_message_index.update(|m| {
        m.remove(&agent_ref);
    });
    state.forget_history_window(&agent_ref);
    state.streaming_text.update(|m| {
        m.remove(&agent_ref);
    });
    state.agent_turn_active.update(|m| {
        m.remove(&agent_ref);
    });
    state.transient_events.update(|m| {
        m.remove(&agent_ref);
    });
    state.task_lists.update(|m| {
        m.remove(&agent_ref);
    });
    state.agent_message_queue.update(|m| {
        m.remove(&agent_ref);
    });
    state.agent_session_settings.update(|m| {
        m.remove(&agent_ref);
    });

    for event in payload.events {
        match event {
            AgentBootstrapEvent::AgentStart(inner) => {
                state.agents.update(|agents| {
                    if let Some(agent) = agents
                        .iter_mut()
                        .find(|a| a.local_host_id == *host && a.agent_id == agent_ref.agent_id)
                    {
                        agent.started = true;
                        if let Some(session_id) = inner.session_id {
                            agent.session_id = Some(session_id);
                        }
                    }
                });
            }
            AgentBootstrapEvent::AgentError(inner) => {
                if inner.fatal {
                    state.agents.update(|agents| {
                        if let Some(agent) = agents
                            .iter_mut()
                            .find(|a| a.local_host_id == *host && a.agent_id == agent_ref.agent_id)
                        {
                            agent.fatal_error = Some(inner.message.clone());
                        }
                    });
                }
                let entry = ChatMessageEntry {
                    message: protocol::ChatMessage {
                        message_id: None,
                        timestamp: js_sys::Date::now() as u64,
                        sender: protocol::MessageSender::Error,
                        content: inner.message,
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
                    map.entry(agent_ref.clone()).or_default().push(entry);
                });
            }
            AgentBootstrapEvent::SessionSettings(inner) => {
                state.agent_session_settings.update(|map| {
                    map.insert(agent_ref.clone(), inner.values);
                });
            }
            AgentBootstrapEvent::QueuedMessages(inner) => {
                state.agent_message_queue.update(|map| {
                    map.insert(agent_ref.clone(), inner.messages);
                });
            }
            AgentBootstrapEvent::ChatEvent(event) => {
                apply_chat_event(state, &agent_ref, event);
            }
        }
    }

    // Window a long restored history: render only the last message, collapsing
    // earlier ones behind a "Load previous conversation history" control. If
    // the restored agent is idle, mark it as "settling" so the floor keeps
    // tracking the tail if the resumed backend trickles the rest of its
    // transcript in as live events after this snapshot (see
    // `AppState::push_chat_message_entry`). The AI still has the full
    // conversation in context — this only changes what the view renders.
    let total = state
        .chat_messages
        .with_untracked(|m| m.get(&agent_ref).map(|v| v.len()).unwrap_or(0));
    let floor = crate::components::chat_view::initial_history_floor(total);
    state.history_floor.update(|m| {
        m.insert(agent_ref.clone(), floor);
    });
    let turn_active = state
        .agent_turn_active
        .with_untracked(|m| m.get(&agent_ref).copied().unwrap_or(false));
    let stream_active = state
        .streaming_text
        .with_untracked(|m| m.contains_key(&agent_ref));
    if !turn_active && !stream_active {
        state.history_settling.update(|s| {
            s.insert(agent_ref.clone());
        });
    }
}

fn apply_project_bootstrap(
    state: &AppState,
    host: &LocalHostId,
    project_id: ProjectId,
    payload: ProjectBootstrapPayload,
) {
    log::info!(
        "dispatch project_bootstrap host={} project_id={} reviews={}",
        host,
        project_id,
        payload.review_summaries.len()
    );
    state.projects.update(|projects| {
        if let Some(existing) = projects
            .iter_mut()
            .find(|e| e.local_host_id == *host && e.project.id == payload.project.id)
        {
            existing.project = payload.project;
        } else {
            projects.push(ProjectInfo {
                local_host_id: host.clone(),
                project: payload.project,
            });
            sort_project_infos(projects);
        }
    });
    // The bootstrap file_list is a full snapshot — drop any prior entries
    // for this project before re-applying so we don't merge stale paths.
    state.file_tree.update(|file_tree| {
        file_tree.remove(&(host.clone(), project_id.clone()));
        apply_project_file_list(file_tree, host, project_id.clone(), payload.file_list);
    });
    state.git_status.update(|git_status| {
        git_status.insert((host.clone(), project_id.clone()), payload.git_status.roots);
    });
    state.review_summaries.update(|map| {
        map.insert((host.clone(), project_id), payload.review_summaries);
    });
}

fn apply_review_bootstrap(
    state: &AppState,
    host: &LocalHostId,
    review_id: ReviewId,
    payload: ReviewBootstrapPayload,
) {
    log::info!(
        "dispatch review_bootstrap host={} review={} comments={} suggestions={}",
        host,
        review_id,
        payload.review.comments.len(),
        payload.review.suggestions.len(),
    );
    apply_review_event(
        state,
        host,
        review_id,
        ReviewEventPayload::Snapshot {
            review: payload.review,
        },
    );
}

fn apply_browse_bootstrap(
    state: &AppState,
    host: &LocalHostId,
    stream: &StreamPath,
    payload: BrowseBootstrapPayload,
) {
    log::info!("dispatch browse_bootstrap host={} stream={}", host, stream);
    let key = (host.clone(), stream.clone());
    state.host_browses.update(|browses| {
        let session = browses
            .entry(key.clone())
            .or_insert_with(|| HostBrowseSession {
                local_host_id: host.clone(),
                stream: stream.clone(),
                opened: None,
                entries_by_path: HashMap::new(),
                latest_error: None,
            });
        session.opened = Some(payload.opened);
        match payload.listing {
            BrowseBootstrapListing::Entries { entries } => {
                session.latest_error = None;
                session
                    .entries_by_path
                    .insert(entries.path.clone(), entries);
            }
            BrowseBootstrapListing::Error { error } => {
                session.latest_error = Some(error);
            }
        }
    });
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn agent_bootstrap_mid_turn_keeps_live_stream_end_visible() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host = LocalHostId("midturn-host".to_owned());
            let agent_id = AgentId("a-midturn".to_owned());
            let agent_ref = AgentRef {
                local_host_id: host.clone(),
                agent_id: agent_id.clone(),
            };
            let stream = StreamPath("/agent/a-midturn/inst".to_owned());

            state.agents.update(|agents| {
                agents.push(AgentInfo {
                    local_host_id: host.clone(),
                    agent_id: agent_id.clone(),
                    name: "Midturn Agent".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    project_id: None,
                    parent_agent_id: None,
                    session_id: None,
                    custom_agent_id: None,
                    created_at_ms: 0,
                    instance_stream: stream.clone(),
                    started: true,
                    fatal_error: None,
                });
            });

            let message =
                |content: String, sender: protocol::MessageSender| protocol::ChatMessage {
                    message_id: None,
                    timestamp: 0,
                    sender,
                    content,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                };
            let mut events = (0..20)
                .map(|index| {
                    AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message(
                        format!("history {index}"),
                        protocol::MessageSender::User,
                    )))
                })
                .collect::<Vec<_>>();
            events.push(AgentBootstrapEvent::ChatEvent(
                ChatEvent::TypingStatusChanged(true),
            ));
            events.push(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(
                protocol::StreamStartData {
                    message_id: Some("midturn-message".to_owned()),
                    agent: "Midturn Agent".to_owned(),
                    model: Some("model".to_owned()),
                },
            )));
            events.push(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamDelta(
                protocol::StreamTextDeltaData {
                    message_id: Some("midturn-message".to_owned()),
                    text: "partial".to_owned(),
                },
            )));

            apply_agent_bootstrap(&state, &host, &stream, AgentBootstrapPayload { events });

            assert!(
                state
                    .agent_turn_active
                    .with_untracked(|map| map.get(&agent_ref).copied().unwrap_or(false)),
                "bootstrap should leave the restored agent mid-turn"
            );
            assert!(
                !state
                    .history_settling
                    .with_untracked(|set| set.contains(&agent_ref)),
                "mid-turn restore must not keep tail-tracking history"
            );
            assert_eq!(
                state
                    .streaming_text
                    .with_untracked(|map| map.get(&agent_ref).map(|s| s.text.get_untracked())),
                Some("partial".to_owned()),
                "active stream delta should be restored"
            );

            apply_chat_event(
                &state,
                &agent_ref,
                ChatEvent::StreamEnd(protocol::StreamEndData {
                    message: message(
                        "finished".to_owned(),
                        protocol::MessageSender::Assistant {
                            agent: "Midturn Agent".to_owned(),
                        },
                    ),
                }),
            );

            assert_eq!(
                state
                    .chat_messages
                    .with_untracked(|map| map.get(&agent_ref).map(Vec::len)),
                Some(21),
                "live StreamEnd should append to the restored transcript"
            );
            assert_eq!(
                state
                    .history_floor
                    .with_untracked(|map| map.get(&agent_ref).copied().unwrap_or(0)),
                0,
                "live StreamEnd must accumulate instead of collapsing behind the history window"
            );
        });
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::prelude::{GetUntracked, WithUntracked};
    use protocol::{
        AgentId, DiffContextMode, Envelope, FrameKind, HostAbsPath, HostBrowseOpenedPayload,
        HostPlatform, ListSessionsPayload, ProjectDiffScope, ProjectFileContentsPayload,
        ProjectGitDiffPayload, ProjectId, ProjectPath, ProjectRootPath, ReviewAiReviewerState,
        ReviewAiReviewerStatus, ReviewComment, ReviewCommentId, ReviewCommentSource,
        ReviewDiffSelection, ReviewEventPayload, ReviewId, ReviewStatus, StreamPath,
        TeamCompactNotifyPayload, TeamCompactStatus, TeamId, TeamMemberId,
    };
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Use only by tests that want the validator state to be unprimed
    /// (e.g. seq-mismatch tests where the first frame must hit the
    /// raw FrontendSeqValidator). Most tests should call `primed_host`.
    fn host(id: &str) -> LocalHostId {
        let host = LocalHostId(id.to_owned());
        reset_inbound_seq_for_host(&host);
        host
    }

    fn primed_host(state: &AppState, id: &str) -> LocalHostId {
        let host = LocalHostId(id.to_owned());
        prime_host_for_tests(state, &host);
        host
    }

    /// Dispatch a synthetic `AgentBootstrap` so subsequent dispatches on
    /// the given agent instance stream pass the bootstrap-first check,
    /// then rewind the per-stream seq counter so the test's first
    /// envelope on that stream can use seq 0.
    fn prime_agent_stream(state: &AppState, host: &LocalHostId, stream: &StreamPath) {
        let bootstrap = protocol::AgentBootstrapPayload { events: Vec::new() };
        let env = Envelope::from_payload(stream.clone(), FrameKind::AgentBootstrap, 0, &bootstrap)
            .expect("synthetic AgentBootstrap");
        dispatch_envelope(state, host, env);
        INBOUND_SEQ.with(|v| {
            v.borrow_mut()
                .expected
                .remove(&(host.clone(), stream.clone()));
        });
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
        let host = primed_host(&state, "mobile-project-foundation");
        let project_id = ProjectId("project-a".to_owned());
        let root = ProjectRootPath("/repo".to_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "src/lib.rs".to_owned(),
        };
        // ProjectBootstrap is required as the first frame on a /project/ stream.
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/project/project-a",
                FrameKind::ProjectBootstrap,
                0,
                &protocol::ProjectBootstrapPayload {
                    project: protocol::Project {
                        id: project_id.clone(),
                        name: "project-a".to_owned(),
                        source: protocol::ProjectSource::Standalone {
                            roots: vec![root.clone()],
                        },
                        sort_order: 0,
                    },
                    file_list: ProjectFileListPayload {
                        incremental: false,
                        roots: Vec::new(),
                    },
                    git_status: ProjectGitStatusPayload { roots: Vec::new() },
                    review_summaries: Vec::new(),
                },
            ),
        );

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/project/project-a",
                FrameKind::ProjectFileContents,
                1,
                &ProjectFileContentsPayload {
                    path: path.clone(),
                    version: protocol::ProjectFileVersion(1),
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
                2,
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
        let host = primed_host(&state, "mobile-review-foundation");
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
                FrameKind::ReviewBootstrap,
                0,
                &protocol::ReviewBootstrapPayload { review },
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
                        anchor_status: protocol::ReviewAnchorStatus::Current,
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
        let host = primed_host(&state, "mobile-compaction-navigation");
        let old_agent_id = AgentId("old-agent".to_owned());
        let new_agent_id = AgentId("new-agent".to_owned());

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-compaction-navigation",
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
                    session_id: None,
                    workflow: None,
                    created_at_ms: 1,
                    instance_stream: StreamPath("/agent/old-agent/inst".to_owned()),
                },
            ),
        );
        prime_agent_stream(
            &state,
            &host,
            &StreamPath("/agent/old-agent/inst".to_owned()),
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
                "/host/mobile-compaction-navigation",
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
                    session_id: None,
                    workflow: None,
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
                "/host/mobile-compaction-navigation",
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
        let host = primed_host(&state, "mobile-team-browse-foundation");

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-team-browse-foundation",
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

        // BrowseBootstrap must be the first frame on a /browse/ stream.
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/browse/browse-a",
                FrameKind::BrowseBootstrap,
                0,
                &protocol::BrowseBootstrapPayload {
                    opened: HostBrowseOpenedPayload {
                        home: HostAbsPath("/Users/mike".to_owned()),
                        root: HostAbsPath("/".to_owned()),
                        separator: '/',
                        platform: HostPlatform::Macos,
                    },
                    listing: protocol::BrowseBootstrapListing::Entries {
                        entries: protocol::HostBrowseEntriesPayload {
                            path: HostAbsPath("/".to_owned()),
                            parent: None,
                            entries: Vec::new(),
                        },
                    },
                },
            ),
        );
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/browse/browse-a",
                FrameKind::HostBrowseOpened,
                1,
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

    #[wasm_bindgen_test]
    fn host_bootstrap_applies_sessions_projects_and_agents() {
        let state = AppState::new();
        let host = LocalHostId("mobile-host-bootstrap".to_owned());
        reset_inbound_seq_for_host(&host);

        // Welcome at seq 0 + HostBootstrap at seq 1 carry the initial
        // host snapshot — no separate ListSessions, ProjectNotify, etc.
        // is required.
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-host-bootstrap",
                FrameKind::Welcome,
                0,
                &protocol::WelcomePayload {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    tyde_version: protocol::TYDE_VERSION,
                    release_version: None,
                },
            ),
        );
        let session = protocol::SessionSummary {
            id: protocol::SessionId("sess-1".to_owned()),
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: Vec::new(),
            project_id: None,
            alias: None,
            user_alias: None,
            parent_id: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            message_count: 0,
            token_count: None,
            resumable: true,
            compacted_from_session_id: None,
            compacted_to_session_id: None,
            compacted_at_ms: None,
            compaction_summary_preview: None,
        };
        let project = protocol::Project {
            id: ProjectId("p-1".to_owned()),
            name: "Project One".to_owned(),
            source: protocol::ProjectSource::Standalone {
                roots: vec![ProjectRootPath("/repo".to_owned())],
            },
            sort_order: 0,
        };
        let agent_payload = protocol::NewAgentPayload {
            agent_id: AgentId("a-1".to_owned()),
            name: "Agent One".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: Vec::new(),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath("/agent/a-1/inst".to_owned()),
        };
        let bootstrap = protocol::HostBootstrapPayload {
            settings: protocol::HostSettings {
                enabled_backends: vec![protocol::BackendKind::Codex],
                default_backend: Some(protocol::BackendKind::Codex),
                enable_mobile_connections: false,
                mobile_broker_url: None,
                tyde_debug_mcp_enabled: false,
                tyde_agent_control_mcp_enabled: true,
                complexity_tiers_enabled: false,
                backend_tier_configs: std::collections::HashMap::new(),
            },
            mobile_access: protocol::MobileAccessStatePayload {
                broker_status: protocol::MobileBrokerStatus::Disabled,
                pairing: protocol::MobilePairingState::Idle,
                paired_devices: Vec::new(),
            },
            backend_setup: protocol::BackendSetupPayload {
                backends: Vec::new(),
            },
            session_schemas: Vec::new(),
            sessions: vec![session.clone()],
            projects: vec![project.clone()],
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            steering: Vec::new(),
            custom_agents: Vec::new(),
            team_preset_catalog: protocol::TeamPresetCatalog {
                role_presets: Vec::new(),
                personality_traits: Vec::new(),
                personality_presets: Vec::new(),
                team_templates: Vec::new(),
            },
            team_drafts: Vec::new(),
            teams: Vec::new(),
            team_members: Vec::new(),
            team_member_bindings: Vec::new(),
            agents: vec![agent_payload.clone()],
            workflow_summaries: Vec::new(),
            workflow_diagnostics: Vec::new(),
            workflow_runs: Vec::new(),
            workflow_locations: Vec::new(),
        };
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-host-bootstrap",
                FrameKind::HostBootstrap,
                1,
                &bootstrap,
            ),
        );

        // Session list is populated for this host without any
        // ListSessions roundtrip.
        let sessions = state.sessions.get_untracked();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].summary.id, session.id);
        assert_eq!(sessions[0].local_host_id, host);

        // Projects landed in the per-host list.
        let projects = state.projects.get_untracked();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project.id, project.id);

        // Agent landed in state.agents without auto-focus (the
        // HostBootstrap path must not run NewAgent's auto-open logic).
        let agents = state.agents.get_untracked();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, agent_payload.agent_id);
        assert!(
            state.active_agent.get_untracked().is_none(),
            "HostBootstrap-delivered agent must not steal active_agent"
        );

        // Host settings landed for this host.
        let settings = state
            .host_settings_by_host
            .with_untracked(|m| m.get(&host).cloned());
        assert!(
            settings.is_some_and(|s| s.default_backend == Some(protocol::BackendKind::Codex)),
            "HostBootstrap should set host settings"
        );
    }

    #[wasm_bindgen_test]
    fn agent_bootstrap_replays_inner_events_through_chat_reducer() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-agent-bootstrap");
        let agent_id = AgentId("a-1".to_owned());
        let instance_stream = StreamPath("/agent/a-1/inst".to_owned());

        // Register the agent so resolve_agent_ref finds it for the
        // AgentBootstrap dispatch.
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-agent-bootstrap",
                FrameKind::NewAgent,
                0,
                &protocol::NewAgentPayload {
                    agent_id: agent_id.clone(),
                    name: "Agent One".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    custom_agent_id: None,
                    team_id: None,
                    team_member_id: None,
                    project_id: None,
                    parent_agent_id: None,
                    session_id: None,
                    workflow: None,
                    created_at_ms: 1,
                    instance_stream: instance_stream.clone(),
                },
            ),
        );

        let agent_start = protocol::AgentStartPayload {
            agent_id: agent_id.clone(),
            name: "Agent One".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: Vec::new(),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
        };
        let chat_event = protocol::ChatEvent::MessageAdded(protocol::ChatMessage {
            message_id: None,
            timestamp: 2,
            sender: protocol::MessageSender::User,
            content: "Hello".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        });
        let bootstrap = protocol::AgentBootstrapPayload {
            events: vec![
                protocol::AgentBootstrapEvent::AgentStart(agent_start),
                protocol::AgentBootstrapEvent::ChatEvent(chat_event),
            ],
        };
        dispatch_envelope(
            &state,
            &host,
            envelope(
                instance_stream.0.as_str(),
                FrameKind::AgentBootstrap,
                0,
                &bootstrap,
            ),
        );

        // AgentStart was replayed → started=true.
        let started = state
            .agents
            .with_untracked(|agents| agents.iter().any(|a| a.agent_id == agent_id && a.started));
        assert!(
            started,
            "AgentStart inside bootstrap should mark agent started"
        );

        // ChatEvent::MessageAdded was replayed through the same reducer
        // as a live ChatEvent → chat_messages has the entry.
        let agent_ref = AgentRef {
            local_host_id: host.clone(),
            agent_id,
        };
        let msgs = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).cloned())
            .unwrap_or_default();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message.content, "Hello");
    }

    #[wasm_bindgen_test]
    fn message_metadata_updated_patches_existing_row_in_place() {
        let state = AppState::new();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId("mobile-meta-host".to_owned()),
            agent_id: AgentId("a-meta".to_owned()),
        };
        let message_id = protocol::ChatMessageId("msg-meta-1".to_owned());

        let initial = protocol::ChatMessage {
            message_id: Some(message_id.clone()),
            timestamp: 1,
            sender: protocol::MessageSender::Assistant {
                agent: "test-agent".to_owned(),
            },
            content: "hello world".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        };
        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::MessageAdded(initial),
        );

        let update = protocol::MessageMetadataUpdateData {
            message_id: message_id.clone(),
            model_info: Some(protocol::ModelInfo {
                model: "gpt-test".to_owned(),
            }),
            token_usage: Some(protocol::TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
                total_tokens: 10,
                cached_prompt_tokens: None,
                cache_creation_input_tokens: None,
                reasoning_tokens: None,
            }),
            context_breakdown: None,
        };
        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::MessageMetadataUpdated(update),
        );

        let rows = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).cloned())
            .expect("agent rows");
        assert_eq!(rows.len(), 1, "metadata update must not append a row");
        assert_eq!(rows[0].message.content, "hello world", "content untouched");
        assert!(
            rows[0]
                .message
                .model_info
                .as_ref()
                .is_some_and(|m| m.model == "gpt-test"),
            "model_info patched"
        );
        assert!(
            rows[0]
                .message
                .token_usage
                .as_ref()
                .is_some_and(|t| t.total_tokens == 10),
            "token_usage patched"
        );
        assert!(
            rows[0].message.context_breakdown.is_none(),
            "None update fields leave existing value alone"
        );

        // A follow-up update that only carries context_breakdown must
        // not stomp on the previously-patched model_info / token_usage.
        let breakdown = protocol::ContextBreakdown {
            system_prompt_bytes: 1,
            tool_io_bytes: 2,
            conversation_history_bytes: 3,
            reasoning_bytes: 4,
            context_injection_bytes: 5,
            input_tokens: 6,
            context_window: 8000,
        };
        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::MessageMetadataUpdated(protocol::MessageMetadataUpdateData {
                message_id: message_id.clone(),
                model_info: None,
                token_usage: None,
                context_breakdown: Some(breakdown),
            }),
        );
        let rows = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).cloned())
            .expect("agent rows");
        assert!(
            rows[0]
                .message
                .model_info
                .as_ref()
                .is_some_and(|m| m.model == "gpt-test"),
            "prior model_info preserved across partial update"
        );
        assert!(
            rows[0]
                .message
                .token_usage
                .as_ref()
                .is_some_and(|t| t.total_tokens == 10),
            "prior token_usage preserved across partial update"
        );
        assert!(
            rows[0]
                .message
                .context_breakdown
                .as_ref()
                .is_some_and(|c| c.context_window == 8000),
            "context_breakdown patched"
        );

        // Unknown message id is a warning, not a crash.
        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::MessageMetadataUpdated(protocol::MessageMetadataUpdateData {
                message_id: protocol::ChatMessageId("missing".to_owned()),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
            }),
        );
        let rows_after = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).map(|r| r.len()).unwrap_or(0));
        assert_eq!(rows_after, 1, "unknown message id must not append a row");
    }

    #[wasm_bindgen_test]
    fn stream_end_then_metadata_updated_patches_existing_row() {
        let state = AppState::new();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId("mobile-stream-meta".to_owned()),
            agent_id: AgentId("a-stream-meta".to_owned()),
        };
        let message_id = protocol::ChatMessageId("msg-stream-1".to_owned());

        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::StreamStart(protocol::StreamStartData {
                message_id: Some(message_id.0.clone()),
                agent: "test-agent".to_owned(),
                model: Some("gpt-test".to_owned()),
            }),
        );

        let chat_message = protocol::ChatMessage {
            message_id: Some(message_id.clone()),
            timestamp: 1,
            sender: protocol::MessageSender::Assistant {
                agent: "test-agent".to_owned(),
            },
            content: "streamed body".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        };
        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::StreamEnd(protocol::StreamEndData {
                message: chat_message,
            }),
        );

        let rows_after_stream = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).map(|r| r.len()).unwrap_or(0));
        assert_eq!(rows_after_stream, 1, "StreamEnd appends one row");

        apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::MessageMetadataUpdated(protocol::MessageMetadataUpdateData {
                message_id: message_id.clone(),
                model_info: Some(protocol::ModelInfo {
                    model: "gpt-test".to_owned(),
                }),
                token_usage: Some(protocol::TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    total_tokens: 10,
                    cached_prompt_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                }),
                context_breakdown: None,
            }),
        );

        let rows = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).cloned())
            .expect("agent rows");
        assert_eq!(
            rows.len(),
            1,
            "MessageMetadataUpdated must not append a row after StreamEnd"
        );
        assert_eq!(rows[0].message.content, "streamed body");
        assert!(
            rows[0]
                .message
                .model_info
                .as_ref()
                .is_some_and(|m| m.model == "gpt-test"),
            "model_info patched in place"
        );
        assert!(
            rows[0]
                .message
                .token_usage
                .as_ref()
                .is_some_and(|t| t.total_tokens == 10),
            "token_usage patched in place"
        );
    }
}
