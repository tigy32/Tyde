use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};

use protocol::MobileAccessErrorCode;
use protocol::types::{AgentCompactNotifyPayload, AgentCompactStatus};
use protocol::{
    AgentActivityStatsPayload, AgentActivitySummaryPayload, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentClosedPayload, AgentErrorPayload, AgentId, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, BackendCapacityPayload, BackendConfigSchemasPayload,
    BackendSetupPayload, BrowseBootstrapListing, BrowseBootstrapPayload, ChatEvent,
    ClientErrorCode, CodeIntelOverviewPayload, CommandErrorPayload, CustomAgentNotifyPayload,
    Envelope, FrameKind, HeartbeatPayload, HostBootstrapPayload, HostBrowseEntriesPayload,
    HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload,
    LaunchProfileCatalogPayload, ListSessionsPayload, McpServerNotifyPayload, NewAgentPayload,
    ProjectBootstrapPayload, ProjectEventPayload, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, QueuedMessagesPayload, RejectCode, RejectPayload, ReviewBootstrapPayload,
    ReviewEventPayload, ReviewId, SeqMismatch, SessionHistoryPayload, SessionListPayload,
    SessionSchemasPayload, SessionSettingsPayload, SkillNotifyPayload, SteeringNotifyPayload,
    StreamPath, TaskTokenUsagePayload, TeamCompactNotifyPayload, TeamCompactStatus,
    TeamDraftNotifyPayload, TeamMemberBindingNotifyPayload, TeamMemberNotifyPayload,
    TeamMemberShuffleSuggestionNotifyPayload, TeamNotifyPayload, TeamPresetCatalogNotifyPayload,
};

use crate::bridge;
use crate::state::MobileShellError;
use crate::state::{
    ActiveAgentRef, AgentInfo, AgentRef, AppState, ChatMessageEntry, ConnectionStatus,
    HostBrowseSession, LocalHostId, ProjectDiffRef, ProjectFileRef, ProjectFileState, ProjectInfo,
    ReviewRef, SessionHistoryState, SessionInfo, SessionListLoadState, StreamingState,
    ToolRequestEntry, TransientEvent, reduce_project_diff_response, sort_project_infos,
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

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
}

pub fn reset_inbound_seq_for_host(host: &LocalHostId) {
    INBOUND_SEQ.with(|validator| validator.borrow_mut().reset_host(host));
}

/// Test helper: prime the inbound validators for `host` so subsequent
/// dispatch calls behave as if the server had already delivered a
/// `Welcome` (seq 0) + `HostBootstrap` (seq 1) pair on the
/// `/host/<host>` stream.
///
/// After priming, the `FrontendSeqValidator` is rewound so tests can
/// dispatch their first envelope at seq `0` without a seq-mismatch error.
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
            background_agent_features: Default::default(),
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Vec::new(),
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
        backend_config_schemas: Vec::new(),
        backend_config_snapshots: Vec::new(),
        launch_profile_catalog: Default::default(),
        sessions: Vec::new(),
        session_list: Default::default(),
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
        task_token_usages: Vec::new(),
        workflow_summaries: Vec::new(),
        workflow_diagnostics: Vec::new(),
        workflow_runs: Vec::new(),
        workflow_locations: Vec::new(),
        agents_view_preferences: None,
    };

    let welcome_env = Envelope::from_payload(host_stream.clone(), FrameKind::Welcome, 0, &welcome)
        .expect("synthetic Welcome");
    dispatch_envelope(state, host, welcome_env);
    let bootstrap_env =
        Envelope::from_payload(host_stream, FrameKind::HostBootstrap, 1, &bootstrap)
            .expect("synthetic HostBootstrap");
    dispatch_envelope(state, host, bootstrap_env);

    // Let the test's next envelope start a fresh sequence.
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
        if envelope.kind == FrameKind::SessionList {
            clear_session_list_loading(state, host);
        }
        report_protocol_error(
            state,
            host,
            bridge::ConnectionInvalidation::SequenceViolation { message },
        );
        return;
    }
    match envelope.kind {
        FrameKind::Welcome => {
            state.command_errors_by_host.update(|map| {
                map.remove(host);
            });
            state.heartbeat_pending_since_by_host.update(|map| {
                map.remove(host);
            });
            state.heartbeat_round_trip_ms_by_host.update(|map| {
                map.remove(host);
            });
            state.connection_statuses.update(|map| {
                map.insert(host.clone(), ConnectionStatus::Bootstrapping);
            });
            // Sessions/projects/teams etc. arrive via HostBootstrap (seq 1
            // on the host stream).  We do not pre-clear anything here because
            // apply_host_bootstrap replaces each collection atomically, so
            // data stays visible until the moment it is refreshed rather than
            // flashing blank between Welcome and HostBootstrap.
            log::info!("connected to host {}", host);
        }
        FrameKind::HostBootstrap => match envelope.parse_payload::<HostBootstrapPayload>() {
            Ok(payload) => apply_host_bootstrap(state, host, &envelope.stream, payload),
            Err(error) => {
                let message = format!(
                    "failed to parse HostBootstrap host={} stream={} seq={}: {}",
                    host, envelope.stream, envelope.seq, error
                );
                log::error!("{message}");
                report_protocol_error(
                    state,
                    host,
                    bridge::ConnectionInvalidation::ProtocolViolation { message },
                );
            }
        },
        FrameKind::HeartbeatAck => match envelope.parse_payload::<HeartbeatPayload>() {
            Ok(payload) => {
                let pending_since = state
                    .heartbeat_pending_since_by_host
                    .with_untracked(|pending| pending.get(host).copied());
                if pending_since == Some(payload.client_sent_at_ms) {
                    state.heartbeat_pending_since_by_host.update(|pending| {
                        pending.remove(host);
                    });
                    let round_trip_ms = unix_time_ms().saturating_sub(payload.client_sent_at_ms);
                    state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
                        round_trips.insert(host.clone(), round_trip_ms);
                    });
                } else {
                    log::warn!(
                        "ignoring stale heartbeat acknowledgment from host {host}: sent_at_ms={}",
                        payload.client_sent_at_ms
                    );
                }
            }
            Err(error) => log::error!(
                "failed to parse HeartbeatAck host={} stream={} seq={}: {}",
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
                apply_reject(state, host, payload);
                clear_session_history_loading_for_host(state, host);
            }
        }
        FrameKind::CommandError => {
            if let Ok(payload) = envelope.parse_payload::<CommandErrorPayload>() {
                let message = format!(
                    "{} failed on {}: {}",
                    payload.operation, payload.stream, payload.message
                );
                log::error!("command error on {host}: {message}");
                if matches!(payload.request_kind, FrameKind::LoadAgent) {
                    // A failed LoadAgent leaves the chat spinning forever
                    // because the bootstrap snapshot never arrives. Surface a
                    // chat-local error row (which retires the spinner) instead
                    // of a host-level banner, so the failure is visible exactly
                    // where the user opened the chat. The load latch is kept so
                    // the auto-load effect does not retry on its next run.
                    surface_load_agent_error(state, host, &payload);
                } else {
                    state.command_errors_by_host.update(|map| {
                        map.insert(host.clone(), message);
                    });
                    if matches!(payload.request_kind, FrameKind::ListSessions) {
                        clear_session_list_loading(state, host);
                    }
                    clear_session_history_loading_on_error(state, host, &payload);
                }
            }
        }
        FrameKind::HostSettings => {
            if let Ok(payload) = envelope.parse_payload::<HostSettingsPayload>() {
                state.host_settings_by_host.update(|map| {
                    map.insert(host.clone(), payload.settings);
                });
            }
        }
        FrameKind::AgentActivitySummary => {
            match envelope.parse_payload::<AgentActivitySummaryPayload>() {
                Ok(_payload) => {}
                Err(error) => log::error!(
                    "failed to parse AgentActivitySummary host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                ),
            }
        }
        FrameKind::TaskTokenUsage => {
            if let Err(error) = envelope.parse_payload::<TaskTokenUsagePayload>() {
                log::error!(
                    "failed to parse TaskTokenUsage host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                );
            }
        }
        FrameKind::BackendSetup => {
            if let Ok(payload) = envelope.parse_payload::<BackendSetupPayload>() {
                state.backend_setup_by_host.update(|map| {
                    map.insert(host.clone(), payload.backends);
                });
            }
        }
        FrameKind::BackendCapacity => match envelope.parse_payload::<BackendCapacityPayload>() {
            Ok(payload) => {
                // Full replacement for this host — the server owns the canonical
                // per-(host, backend) snapshot and re-emits it on every change,
                // so mobile holds no history and merges nothing.
                state.backend_capacity_by_host.update(|by_host| {
                    let host_capacity = by_host.entry(host.clone()).or_default();
                    host_capacity.clear();
                    for snapshot in payload.snapshots {
                        host_capacity.insert(snapshot.backend_kind, snapshot);
                    }
                });
            }
            Err(error) => log::error!(
                "failed to parse BackendCapacity host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::BackendConfigSchemas => {
            if let Err(error) = envelope.parse_payload::<BackendConfigSchemasPayload>() {
                log::error!(
                    "failed to parse BackendConfigSchemas host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                );
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
        FrameKind::SessionHistory => match envelope.parse_payload::<SessionHistoryPayload>() {
            Ok(payload) => apply_session_history(state, host, &envelope.stream, payload),
            Err(error) => log::error!(
                "failed to parse SessionHistory host={} stream={} seq={}: {}",
                host,
                envelope.stream,
                envelope.seq,
                error
            ),
        },
        FrameKind::SessionList => match envelope.parse_payload::<SessionListPayload>() {
            Ok(payload) => {
                apply_session_list_page(state, host, payload);
            }
            Err(error) => {
                let message = format!(
                    "failed to parse SessionList payload host={} stream={} seq={}: {}",
                    host, envelope.stream, envelope.seq, error
                );
                log::error!("{message}");
                state.command_errors_by_host.update(|map| {
                    map.insert(host.clone(), message.clone());
                });
                clear_session_list_loading(state, host);
                report_protocol_error(
                    state,
                    host,
                    bridge::ConnectionInvalidation::ProtocolViolation { message },
                );
            }
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
                    // Per-file version advances drive the desktop editor's
                    // in-place reload of open files; the mobile client doesn't
                    // track open-file versions for code-intel, so there is
                    // nothing to refresh here.
                    ProjectEventPayload::FilesChanged { .. } => {}
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
        // Desktop-only surfaces (the await progress card, the Files-explorer
        // code-intel footer, and launch-profile menus). Mobile has no UI for
        // these, so parse to validate the wire shape and intentionally drop
        // them — quietly, without the "unhandled frame kind" warning below.
        FrameKind::AgentActivityStats => {
            if let Err(error) = envelope.parse_payload::<AgentActivityStatsPayload>() {
                log::error!(
                    "failed to parse AgentActivityStats host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                );
            }
        }
        FrameKind::CodeIntelOverview => {
            if let Err(error) = envelope.parse_payload::<CodeIntelOverviewPayload>() {
                log::error!(
                    "failed to parse CodeIntelOverview host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                );
            }
        }
        FrameKind::LaunchProfileCatalogNotify => {
            if let Err(error) = envelope.parse_payload::<LaunchProfileCatalogPayload>() {
                log::error!(
                    "failed to parse LaunchProfileCatalogNotify host={} stream={} seq={}: {}",
                    host,
                    envelope.stream,
                    envelope.seq,
                    error
                );
            }
        }
        _ => {
            log::warn!("unhandled frame kind: {}", envelope.kind);
        }
    }
}

fn unix_time_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    return js_sys::Date::now().max(0.0) as u64;

    #[cfg(not(target_arch = "wasm32"))]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Surface a sequence/protocol violation **and terminate the connection**.
///
/// Dropping the offending frame and returning is what wedged the stream: the
/// frontend's expected sequence number is not advanced, so every later frame on
/// that stream mismatches too, and the stream is dead forever while the UI keeps
/// waiting. The server's own validator terminates the connection on a violation;
/// this makes the client agree. A terminated connection reconnects on the
/// existing backoff and rebootstraps from authoritative state, which is
/// recoverable — a silently wedged stream is not.
fn report_protocol_error(
    state: &AppState,
    host: &LocalHostId,
    invalidation: bridge::ConnectionInvalidation,
) {
    let message = invalidation.to_string();
    state.connection_statuses.update(|map| {
        map.insert(host.clone(), ConnectionStatus::Error(message.clone()));
    });
    // Report the violation back to the host so the server logs it, before the
    // connection goes away. The frame that triggered this already parsed
    // cleanly, so there is no raw offending line to forward — the structured
    // message carries the detail. Sent on the host stream via the shared
    // outbound seq counter, so it cannot itself re-enter inbound validation and
    // loop. Best-effort: the invalidation below may tear the connection down
    // before it is written, and the server validates independently anyway.
    emit_client_protocol_error(state, host, message.clone());
    if let Err(error) = bridge::invalidate_host_connection(host, invalidation) {
        // Already gone — the outcome we wanted. Nothing to escalate.
        log::warn!("connection already unavailable while invalidating host={host}: {error}");
    }
    state.mobile_shell_error.set(Some(MobileShellError {
        code: MobileAccessErrorCode::BrokerProtocol,
        message,
    }));
}

fn clear_session_list_loading(state: &AppState, host: &LocalHostId) {
    state.session_lists_by_host.update(|map| {
        if let Some(list) = map.get_mut(host) {
            list.loading_more = false;
        }
    });
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
    state.forget_session_history(agent_ref);
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

fn clear_session_history_loading_on_error(
    state: &AppState,
    host: &LocalHostId,
    payload: &CommandErrorPayload,
) {
    if !matches!(payload.request_kind, FrameKind::FetchSessionHistory) {
        return;
    }
    let Some(agent_ref) = resolve_agent_ref(state, host, &payload.stream) else {
        log::warn!(
            "fetch_session_history error on unknown stream host={} stream={}",
            host,
            payload.stream
        );
        return;
    };
    state.session_history.update(|map| {
        if let Some(history) = map.get_mut(&agent_ref) {
            history.loading = false;
        }
    });
}

/// Handle a `CommandError` whose `request_kind` is `LoadAgent`. The mobile chat
/// shows a spinner while the load is latched and no snapshot has arrived; a
/// failed load would otherwise spin forever, because `agent_loaded` is only ever
/// written by a *successful* `AgentBootstrap`.
///
/// This records a typed load error, which is the spinner's terminal state, and
/// pushes a visible error row into the transcript so the failure is legible in
/// the conversation itself.
///
/// Deliberately keep the `agent_load_requests` latch set (and leave
/// `agent_loaded` unset, since no snapshot arrived). `ChatView`'s auto-load
/// effect re-sends `LoadAgent` whenever the active agent is *absent* from
/// `agent_load_requests`. That effect re-runs on `active_agent`/`agents`
/// changes, so clearing the latch here would let the next run re-send:
/// `LoadAgent` -> conflict -> another error row, with the latch cleared each
/// time. The retained latch is the thing that stops the retry whenever the
/// effect runs. Recovery is a deliberate reconnect, which clears both the latch
/// and the error and re-loads on a fresh instance stream.
fn surface_load_agent_error(state: &AppState, host: &LocalHostId, payload: &CommandErrorPayload) {
    let message = format!("Failed to load conversation: {}", payload.message);
    let Some(agent_ref) = resolve_agent_ref(state, host, &payload.stream) else {
        log::warn!(
            "load_agent error on unknown stream host={} stream={}",
            host,
            payload.stream
        );
        // The stream maps to no agent we know, so the error cannot be attributed
        // to one — and guessing which chat it belongs to is exactly the
        // inference this client does not do. Surface it at the host level so it
        // is still visible. The spinner is not stranded by this: `ChatView` will
        // not spin for an agent that is absent from `state.agents`, which is the
        // only way this branch is reachable for an agent the user has open.
        state.command_errors_by_host.update(|map| {
            map.insert(host.clone(), message);
        });
        return;
    };
    state.agent_load_errors.update(|map| {
        map.insert(agent_ref.clone(), message.clone());
    });
    let entry = ChatMessageEntry {
        message: protocol::ChatMessage {
            message_id: None,
            timestamp: js_sys::Date::now() as u64,
            sender: protocol::MessageSender::Error,
            content: message,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        },
        tool_requests: Vec::new(),
    };
    state.push_chat_message_entry(&agent_ref, entry);
}

fn clear_session_history_loading_for_host(state: &AppState, host: &LocalHostId) {
    state.session_history.update(|map| {
        for (agent_ref, history) in map.iter_mut() {
            if agent_ref.local_host_id == *host {
                history.loading = false;
            }
        }
    });
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
        }
        ChatEvent::MessageAdded(message) => {
            state.push_chat_message_entry(
                &agent_ref,
                ChatMessageEntry {
                    message,
                    tool_requests: Vec::new(),
                },
            );
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
            state.streaming_text.update(|map| {
                map.insert(
                    agent_ref.clone(),
                    StreamingState {
                        agent_name: data.agent,
                        model: data.model,
                        text: leptos::prelude::ArcRwSignal::new(String::new()),
                        reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                        tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
                    },
                );
            });
        }
        ChatEvent::StreamDelta(data) => {
            if let Some(streaming) = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned())
            {
                streaming.text.update(|text| text.push_str(&data.text));
            }
        }
        ChatEvent::StreamReasoningDelta(data) => {
            if let Some(streaming) = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned())
            {
                streaming
                    .reasoning
                    .update(|reasoning| reasoning.push_str(&data.text));
            }
        }
        ChatEvent::StreamEnd(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_ref).cloned());
            let tool_requests = streaming
                .as_ref()
                .map(|streaming| streaming.tool_requests.get_untracked())
                .unwrap_or_default();
            state.streaming_text.update(|map| {
                map.remove(&agent_ref);
            });
            state.push_chat_message_entry(
                &agent_ref,
                ChatMessageEntry {
                    message: data.message,
                    tool_requests,
                },
            );
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
        ChatEvent::Orchestration(data) => {
            log::trace!(
                "mobile dispatch chat_event host={} agent_id={} type=orchestration orchestration_agent_id={} orchestration_agent_type={} payload={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                data.agent_id,
                data.agent_type,
                data.payload.kind()
            );
        }
    }
}

fn apply_session_history(
    state: &AppState,
    host: &LocalHostId,
    _stream: &StreamPath,
    payload: SessionHistoryPayload,
) {
    let agent_ref = AgentRef {
        local_host_id: host.clone(),
        agent_id: payload.agent_id.clone(),
    };
    let mut replay = MobileHistoryReplay::default();
    for event in payload.events {
        replay.apply(event, host, &agent_ref);
    }

    if !replay.rows.is_empty() {
        state.chat_messages.update(|map| {
            let current = map.remove(&agent_ref).unwrap_or_default();
            let mut combined = replay.rows;
            combined.extend(current);
            map.insert(agent_ref.clone(), combined);
        });
        rebuild_chat_message_index(state, &agent_ref);
    }

    state.session_history.update(|map| {
        let mut remove = false;
        if let Some(history) = map.get_mut(&agent_ref) {
            history.oldest_seq = payload.oldest_seq;
            history.has_more_before = payload.has_more_before;
            history.loading = false;
            history.message_count = 0;
            if !history.has_more_before {
                remove = true;
            }
        } else if payload.has_more_before {
            map.insert(
                agent_ref.clone(),
                SessionHistoryState {
                    message_count: 0,
                    oldest_seq: payload.oldest_seq,
                    has_more_before: payload.has_more_before,
                    loading: false,
                },
            );
        }
        if remove {
            map.remove(&agent_ref);
        }
    });
}

#[derive(Default)]
struct MobileHistoryReplay {
    rows: Vec<ChatMessageEntry>,
    message_index: HashMap<protocol::ChatMessageId, usize>,
    tool_index: HashMap<String, usize>,
}

impl MobileHistoryReplay {
    fn apply(&mut self, event: ChatEvent, host: &LocalHostId, agent_ref: &AgentRef) {
        match event {
            ChatEvent::MessageAdded(message) => {
                self.push_entry(ChatMessageEntry {
                    message,
                    tool_requests: Vec::new(),
                });
            }
            ChatEvent::MessageMetadataUpdated(data) => {
                let Some(index) = self.message_index.get(&data.message_id).copied() else {
                    return;
                };
                let Some(row) = self.rows.get_mut(index) else {
                    return;
                };
                if data.model_info.is_some() {
                    row.message.model_info = data.model_info;
                }
                if data.token_usage.is_some() {
                    row.message.token_usage = data.token_usage;
                }
                if data.context_breakdown.is_some() {
                    row.message.context_breakdown = data.context_breakdown;
                }
            }
            ChatEvent::StreamEnd(data) => {
                self.push_entry(ChatMessageEntry {
                    message: data.message,
                    tool_requests: Vec::new(),
                });
            }
            ChatEvent::ToolRequest(request) => {
                let tool_name = request.tool_name.clone();
                let tool_call_id = request.tool_call_id.clone();
                let tool_entry = ToolRequestEntry {
                    request,
                    result: None,
                };
                let last_index = self.rows.len().saturating_sub(1);
                if let Some(last) = self.rows.last_mut() {
                    last.tool_requests.push(tool_entry);
                    self.tool_index.insert(tool_call_id, last_index);
                } else {
                    log::error!(
                        "mobile history tool request dropped: tool '{}' (call_id={}) for host {} agent {} without a message row",
                        tool_name,
                        tool_call_id,
                        host,
                        agent_ref.agent_id
                    );
                }
            }
            ChatEvent::ToolExecutionCompleted(data) => {
                let Some(index) = self.tool_index.get(&data.tool_call_id).copied() else {
                    return;
                };
                let Some(row) = self.rows.get_mut(index) else {
                    return;
                };
                if let Some(tool) = row
                    .tool_requests
                    .iter_mut()
                    .find(|tool| tool.request.tool_call_id == data.tool_call_id)
                {
                    tool.result = Some(data);
                }
            }
            ChatEvent::TypingStatusChanged(_)
            | ChatEvent::StreamStart(_)
            | ChatEvent::StreamDelta(_)
            | ChatEvent::StreamReasoningDelta(_)
            | ChatEvent::ToolProgress(_)
            | ChatEvent::TaskUpdate(_)
            | ChatEvent::OperationCancelled(_)
            | ChatEvent::RetryAttempt(_)
            | ChatEvent::Orchestration(_) => {}
        }
    }

    fn push_entry(&mut self, entry: ChatMessageEntry) {
        let index = self.rows.len();
        if let Some(message_id) = entry.message.message_id.clone() {
            self.message_index.entry(message_id).or_insert(index);
        }
        for tool in &entry.tool_requests {
            self.tool_index
                .insert(tool.request.tool_call_id.clone(), index);
        }
        self.rows.push(entry);
    }
}

fn rebuild_chat_message_index(state: &AppState, agent_ref: &AgentRef) {
    let next = state.chat_messages.with_untracked(|messages| {
        messages.get(agent_ref).map(|rows| {
            rows.iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    entry
                        .message
                        .message_id
                        .clone()
                        .map(|message_id| (message_id, index))
                })
                .collect::<HashMap<protocol::ChatMessageId, usize>>()
        })
    });
    state.chat_message_index.update(|indexes| {
        if let Some(next) = next {
            indexes.insert(agent_ref.clone(), next);
        } else {
            indexes.remove(agent_ref);
        }
    });
}

// A `Reject` frame is the host's answer to our `Hello`: the app-level Tyde
// handshake failed before any `Welcome`/`HostBootstrap` could arrive. An
// `IncompatibleProtocol` reject is terminal for this build against this host,
// so it becomes a sticky [`ConnectionStatus::UpdateRequired`] (which transport
// reconnect statuses cannot overwrite — see `app::apply_connection_status`)
// rather than a transient error the reconnect loop would immediately paper
// over with `Connecting`/`Connected`. On the web/PWA we additionally ask the
// loader to self-heal by rebooting into the host's exact published bundle,
// keyed on the reject's `release_version`, so an already-paired host recovers
// without a re-scan. Native shells have no loader to reboot, so the sticky
// error is the surface until the app itself is updated.
fn apply_reject(state: &AppState, host: &LocalHostId, payload: RejectPayload) {
    log::error!(
        "connection rejected on {host}: {} (code={:?}, host protocol {}, app protocol {})",
        payload.message,
        payload.code,
        payload.server_protocol_version,
        protocol::PROTOCOL_VERSION
    );
    match payload.code {
        RejectCode::IncompatibleProtocol => {
            state.connection_statuses.update(|map| {
                map.insert(
                    host.clone(),
                    ConnectionStatus::UpdateRequired {
                        host_protocol: payload.server_protocol_version,
                        app_protocol: protocol::PROTOCOL_VERSION,
                        release_version: payload.release_version.clone(),
                    },
                );
            });
            if let Some(release_version) = payload.release_version.as_ref() {
                crate::bridge::request_loader_repair_version(release_version.as_str());
            }
            // The Connected transport event that preceded this reject already
            // allocated a host stream, seq state, and a connection-instance id
            // (Connected arrives before Hello is answered). Tear that runtime
            // down so the sticky UpdateRequired leaves no dangling connection a
            // later transport event could reuse, and so a self-heal reload or a
            // post-update reconnect starts from a clean protocol session.
            state.host_streams.update(|map| {
                map.remove(host);
            });
            state.active_connection_instance_ids.update(|map| {
                map.remove(host);
            });
            crate::send::reset_seq_for_host(host);
            reset_inbound_seq_for_host(host);
        }
        RejectCode::InvalidHandshake => {
            state.connection_statuses.update(|map| {
                map.insert(host.clone(), ConnectionStatus::Error(payload.message));
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

fn apply_session_list_page(state: &AppState, host: &LocalHostId, payload: SessionListPayload) {
    let page = payload.page;
    let page_session_count = payload.sessions.len();
    log::info!(
        "mobile_apply_session_list host={} cursor={} count={} total={}",
        host,
        page.cursor.offset,
        page_session_count,
        page.total_count
    );
    state.sessions.update(|sessions| {
        if page.cursor.offset == 0 {
            sessions.retain(|s| s.local_host_id != *host);
        }
        for summary in payload.sessions {
            if let Some(existing) = sessions
                .iter_mut()
                .find(|s| s.local_host_id == *host && s.summary.id == summary.id)
            {
                existing.summary = summary;
            } else {
                sessions.push(SessionInfo {
                    local_host_id: host.clone(),
                    summary,
                });
            }
        }
    });
    let loaded_count = state
        .sessions
        .with_untracked(|sessions| sessions.iter().filter(|s| s.local_host_id == *host).count());
    state.session_lists_by_host.update(|map| {
        map.insert(
            host.clone(),
            SessionListLoadState::from_page(page, loaded_count),
        );
    });
}

/// Request the next page of sessions for `host`, in response to an explicit
/// user action ("Load older sessions"). The mobile client renders only the
/// first server-provided page and never auto-drains the rest; this is the one
/// path that fetches more. It reuses the server-owned cursor, limit, and
/// authoritative scope from the currently-loaded page so the follow-up query
/// stays coherent with what the UI is showing.
pub fn load_next_session_page(state: &AppState, host: &LocalHostId) {
    let Some(list) = state
        .session_lists_by_host
        .with_untracked(|map| map.get(host).cloned())
    else {
        return;
    };
    if list.loading_more {
        return;
    }
    let page = list.page;
    let Some(next_cursor) = page.next_cursor() else {
        return;
    };
    let Some(stream) = state.host_stream_untracked(host) else {
        let message = format!(
            "cannot request session page {} for {host}: no host stream",
            next_cursor.offset
        );
        log::error!("{message}");
        state.command_errors_by_host.update(|map| {
            map.insert(host.clone(), message);
        });
        return;
    };
    state.session_lists_by_host.update(|map| {
        if let Some(list) = map.get_mut(host) {
            list.loading_more = true;
        }
    });
    let host = host.clone();
    let state_for_error = state.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let payload = ListSessionsPayload {
            scope: Some(page.scope),
            cursor: Some(next_cursor),
            limit: Some(page.limit),
        };
        if let Err(error) =
            crate::send::send_frame(&host, stream, FrameKind::ListSessions, &payload).await
        {
            let message = format!(
                "failed to request session page {} for {host}: {error}",
                next_cursor.offset
            );
            log::error!("{message}");
            state_for_error.command_errors_by_host.update(|map| {
                map.insert(host.clone(), message);
            });
            state_for_error.session_lists_by_host.update(|map| {
                if let Some(list) = map.get_mut(&host) {
                    list.loading_more = false;
                }
            });
        }
    });
}

fn apply_host_bootstrap(
    state: &AppState,
    host: &LocalHostId,
    stream: &StreamPath,
    payload: HostBootstrapPayload,
) {
    if let Some(current) = state.host_stream_untracked(host) {
        if &current != stream {
            let message = format!(
                "HostBootstrap host={} stream={} does not match active host stream {}",
                host, stream, current
            );
            log::error!("{message}");
            report_protocol_error(
                state,
                host,
                bridge::ConnectionInvalidation::ProtocolViolation { message },
            );
            return;
        }
    } else {
        state.host_streams.update(|map| {
            map.insert(host.clone(), stream.clone());
        });
    }

    let session_page = payload.session_list;
    let bootstrap_session_count = payload.sessions.len();
    log::info!(
        "dispatch host_bootstrap host={} sessions={} total_sessions={} projects={} agents={} teams={}",
        host,
        bootstrap_session_count,
        session_page.total_count,
        payload.projects.len(),
        payload.agents.len(),
        payload.teams.len(),
    );

    state.bootstrapped_host_streams.update(|map| {
        map.insert(host.clone(), stream.clone());
    });
    state.connection_statuses.update(|map| {
        map.insert(host.clone(), ConnectionStatus::Connected);
    });
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
    state.session_lists_by_host.update(|map| {
        map.insert(
            host.clone(),
            SessionListLoadState::from_page(session_page, bootstrap_session_count),
        );
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
    // Prune prior-history state for agents on this host the snapshot no longer
    // knows about, so a dropped agent doesn't leave an orphaned indicator.
    let dropped_refs: Vec<AgentRef> = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .filter(|a| a.local_host_id == *host && !snapshot_ids.contains(&a.agent_id))
            .map(|a| a.agent_ref())
            .collect()
    });
    for dropped in &dropped_refs {
        state.forget_session_history(dropped);
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
    // An authoritative snapshot retires any earlier load failure for this agent.
    state.agent_load_errors.update(|m| {
        m.remove(&agent_ref);
    });
    // Replace prior per-agent chat/stream/queue/task state so the bootstrap
    // snapshot is authoritative.
    state.chat_messages.update(|m| {
        m.remove(&agent_ref);
    });
    state.chat_message_index.update(|m| {
        m.remove(&agent_ref);
    });
    state.forget_session_history(&agent_ref);
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
            AgentBootstrapEvent::HasPriorHistory {
                message_count,
                before_seq,
            } => {
                if message_count > 0 {
                    state.session_history.update(|map| {
                        map.insert(
                            agent_ref.clone(),
                            SessionHistoryState {
                                message_count,
                                oldest_seq: Some(before_seq),
                                has_more_before: true,
                                loading: false,
                            },
                        );
                    });
                }
            }
            AgentBootstrapEvent::ChatEvent(event) => {
                apply_chat_event(state, &agent_ref, event);
            }
            // Mobile does not surface the agent-control activity stats line
            // (no await progress card UX), mirroring how it drops the
            // `AgentActivitySummary` frame above.
            AgentBootstrapEvent::AgentActivityStats(_) => {}
        }
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
        ChatEvent::Orchestration(_) => "Orchestration",
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
                    message_id: Some(protocol::ChatMessageId("midturn-message".to_owned())),
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
            let mut events = vec![AgentBootstrapEvent::HasPriorHistory {
                message_count: 20,
                before_seq: 42,
            }];
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

            apply_agent_bootstrap(
                &state,
                &host,
                &stream,
                AgentBootstrapPayload {
                    events,
                    latest_output: Default::default(),
                },
            );

            assert!(
                state
                    .agent_turn_active
                    .with_untracked(|map| map.get(&agent_ref).copied().unwrap_or(false)),
                "bootstrap should leave the restored agent mid-turn"
            );
            assert!(
                state
                    .session_history
                    .with_untracked(|map| map
                        .get(&agent_ref)
                        .is_some_and(
                            |history| history.message_count == 20 && history.oldest_seq == Some(42)
                        )),
                "bootstrap should keep the server-owned prior-history gate"
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
                Some(1),
                "live StreamEnd should append while prior history remains unloaded"
            );
        });
    }

    #[test]
    fn session_history_uses_payload_owner_and_server_order() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host = LocalHostId("history-host".to_owned());
            let payload_agent = AgentId("payload-agent".to_owned());
            let agent_ref = AgentRef {
                local_host_id: host.clone(),
                agent_id: payload_agent.clone(),
            };
            let message = |id: &str, content: &str| protocol::ChatMessage {
                message_id: Some(protocol::ChatMessageId(id.to_owned())),
                timestamp: 0,
                sender: protocol::MessageSender::Assistant {
                    agent: "History Agent".to_owned(),
                },
                content: content.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            };

            apply_session_history(
                &state,
                &host,
                &StreamPath("/agent/different-stream-owner/inst".to_owned()),
                SessionHistoryPayload {
                    agent_id: payload_agent,
                    events: vec![
                        ChatEvent::MessageAdded(message("first", "first delivered")),
                        ChatEvent::MessageAdded(message("second", "second delivered")),
                    ],
                    has_more_before: false,
                    oldest_seq: None,
                },
            );

            let rows = state
                .chat_messages
                .with_untracked(|map| map.get(&agent_ref).cloned())
                .expect("payload-owned history rows");
            assert_eq!(
                rows.iter()
                    .map(|entry| entry.message.content.as_str())
                    .collect::<Vec<_>>(),
                vec!["first delivered", "second delivered"]
            );
            assert_eq!(
                rows.iter()
                    .filter_map(|entry| entry.message.message_id.as_ref().map(|id| id.0.as_str()))
                    .collect::<Vec<_>>(),
                vec!["first", "second"]
            );
        });
    }

    #[test]
    fn background_tool_completion_updates_prior_message_during_later_stream() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: LocalHostId("background-host".to_owned()),
                agent_id: AgentId("background-agent".to_owned()),
            };
            let assistant_message = |id: &str, content: &str| protocol::ChatMessage {
                message_id: Some(protocol::ChatMessageId(id.to_owned())),
                timestamp: 0,
                sender: protocol::MessageSender::Assistant {
                    agent: "codex".to_owned(),
                },
                content: content.to_owned(),
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
                ChatEvent::StreamEnd(protocol::StreamEndData {
                    message: assistant_message("message-first", "starting"),
                }),
            );
            apply_chat_event(
                &state,
                &agent_ref,
                ChatEvent::ToolRequest(protocol::ToolRequest {
                    tool_call_id: "tool-background".to_owned(),
                    tool_name: "run_command".to_owned(),
                    tool_type: protocol::ToolRequestType::RunCommand {
                        command: "sleep 12".to_owned(),
                        working_directory: "/tmp".to_owned(),
                    },
                }),
            );
            apply_chat_event(
                &state,
                &agent_ref,
                ChatEvent::StreamStart(protocol::StreamStartData {
                    message_id: Some("message-later".to_owned()),
                    agent: "codex".to_owned(),
                    model: Some("gpt-5.6-luna".to_owned()),
                }),
            );
            apply_chat_event(
                &state,
                &agent_ref,
                ChatEvent::ToolExecutionCompleted(protocol::ToolExecutionCompletedData {
                    tool_call_id: "tool-background".to_owned(),
                    tool_name: "run_command".to_owned(),
                    tool_result: protocol::ToolExecutionResult::RunCommand {
                        exit_code: 0,
                        stdout: "done".to_owned(),
                        stderr: String::new(),
                    },
                    success: true,
                    error: None,
                    normalization_failure: None,
                }),
            );

            let first_message_has_result = state.chat_messages.with_untracked(|messages| {
                messages.get(&agent_ref).and_then(|rows| {
                    rows.first().map(|row| {
                        row.tool_requests.first().is_some_and(|tool| {
                            tool.request.tool_call_id == "tool-background"
                                && tool.result.as_ref().is_some_and(|result| result.success)
                        })
                    })
                })
            });
            assert_eq!(first_message_has_result, Some(true));
            assert!(
                state
                    .streaming_text
                    .with_untracked(|streams| streams.contains_key(&agent_ref)),
                "the later assistant response must remain independently active"
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
        let bootstrap = protocol::AgentBootstrapPayload {
            events: Vec::new(),
            latest_output: Default::default(),
        };
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
    fn heartbeat_ack_records_end_to_end_round_trip() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-heartbeat");
        let sent_at_ms = unix_time_ms().saturating_sub(42);
        state.heartbeat_pending_since_by_host.update(|pending| {
            pending.insert(host.clone(), sent_at_ms);
        });

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-heartbeat",
                FrameKind::HeartbeatAck,
                0,
                &HeartbeatPayload {
                    client_sent_at_ms: sent_at_ms,
                },
            ),
        );

        assert!(
            !state
                .heartbeat_pending_since_by_host
                .with_untracked(|pending| pending.contains_key(&host))
        );
        assert!(
            state
                .heartbeat_round_trip_ms_by_host
                .with_untracked(|round_trips| round_trips.get(&host).copied())
                .is_some_and(|round_trip_ms| round_trip_ms >= 42)
        );
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
                &ListSessionsPayload::default(),
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
    fn list_sessions_command_error_clears_loading_state() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-session-list-error");
        state.session_lists_by_host.update(|lists| {
            let mut list = SessionListLoadState::from_page(
                protocol::SessionListPageInfo {
                    scope: protocol::SessionListScope::RootSessions,
                    cursor: protocol::SessionListCursor {
                        generation: protocol::SessionListGeneration(1),
                        offset: 0,
                    },
                    limit: 64,
                    total_count: 100,
                    status: protocol::SessionListPageStatus::More {
                        next_cursor: protocol::SessionListCursor {
                            generation: protocol::SessionListGeneration(1),
                            offset: 64,
                        },
                    },
                },
                64,
            );
            // A user-triggered "load more" is in flight when the error arrives.
            list.loading_more = true;
            lists.insert(host.clone(), list);
        });

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-session-list-error",
                FrameKind::CommandError,
                0,
                &protocol::CommandErrorPayload {
                    stream: StreamPath("/host/mobile-session-list-error".to_owned()),
                    request_kind: FrameKind::ListSessions,
                    setting_target: None,
                    operation: "list_sessions".to_owned(),
                    code: protocol::CommandErrorCode::InvalidInput,
                    message: "stale session list cursor generation 1".to_owned(),
                    fatal: false,
                },
            ),
        );

        assert!(
            state
                .session_lists_by_host
                .with_untracked(|lists| lists.get(&host).is_some_and(|list| !list.loading_more)),
            "ListSessions CommandError should clear the loading-more state"
        );
        assert!(
            state.command_errors_by_host.with_untracked(|errors| errors
                .get(&host)
                .is_some_and(|message| message.contains("stale session list cursor"))),
            "ListSessions CommandError should remain user-visible"
        );
    }

    #[wasm_bindgen_test]
    fn malformed_session_list_clears_loading_state() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-session-list-parse-error");
        state.session_lists_by_host.update(|lists| {
            let mut list = SessionListLoadState::from_page(
                protocol::SessionListPageInfo {
                    scope: protocol::SessionListScope::RootSessions,
                    cursor: protocol::SessionListCursor {
                        generation: protocol::SessionListGeneration(1),
                        offset: 0,
                    },
                    limit: 64,
                    total_count: 100,
                    status: protocol::SessionListPageStatus::More {
                        next_cursor: protocol::SessionListCursor {
                            generation: protocol::SessionListGeneration(1),
                            offset: 64,
                        },
                    },
                },
                64,
            );
            // A user-triggered "load more" is in flight when the error arrives.
            list.loading_more = true;
            lists.insert(host.clone(), list);
        });

        dispatch_envelope(
            &state,
            &host,
            Envelope {
                stream: StreamPath("/host/mobile-session-list-parse-error".to_owned()),
                kind: FrameKind::SessionList,
                seq: 0,
                payload: serde_json::json!({
                    "sessions": null,
                    "page": {
                        "cursor": { "generation": 1, "offset": 0 },
                        "limit": 64,
                        "total_count": 100,
                        "status": { "kind": "complete" }
                    }
                }),
            },
        );

        assert!(
            state
                .session_lists_by_host
                .with_untracked(|lists| lists.get(&host).is_some_and(|list| !list.loading_more)),
            "malformed SessionList should clear the loading-more state"
        );
        assert!(matches!(
            state.connection_statuses.get_untracked().get(&host),
            Some(ConnectionStatus::Error(message))
                if message.contains("failed to parse SessionList payload")
        ));
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
                    launch_profile_id: None,
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
                    activity_summary: Default::default(),
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
                    launch_profile_id: None,
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
                    activity_summary: Default::default(),
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
            launch_profile_id: None,
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
            launch_profile_id: None,
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
            activity_summary: Default::default(),
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
                background_agent_features: Default::default(),
                code_intel: Default::default(),
                backend_config: std::collections::HashMap::new(),
                launch_profiles: Vec::new(),
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
            backend_config_schemas: Vec::new(),
            backend_config_snapshots: Vec::new(),
            launch_profile_catalog: Default::default(),
            sessions: vec![session.clone()],
            session_list: protocol::SessionListPageInfo {
                scope: protocol::SessionListScope::RootSessions,
                cursor: protocol::SessionListCursor {
                    generation: protocol::SessionListGeneration(1),
                    offset: 0,
                },
                limit: 1,
                total_count: 1,
                status: protocol::SessionListPageStatus::Complete,
            },
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
            task_token_usages: Vec::new(),
            workflow_summaries: Vec::new(),
            workflow_diagnostics: Vec::new(),
            workflow_runs: Vec::new(),
            workflow_locations: Vec::new(),
            agents_view_preferences: None,
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
                    launch_profile_id: None,
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
                    activity_summary: Default::default(),
                },
            ),
        );

        let agent_start = protocol::AgentStartPayload {
            agent_id: agent_id.clone(),
            name: "Agent One".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            launch_profile_id: None,
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
            latest_output: Default::default(),
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

    /// Register an agent on `host` whose `LoadAgent` instance stream is
    /// `instance_stream`, returning its `AgentRef`.
    fn register_agent(
        state: &AppState,
        host: &LocalHostId,
        host_stream: &str,
        seq: u64,
        agent_id: &str,
        instance_stream: &StreamPath,
    ) -> AgentRef {
        dispatch_envelope(
            state,
            host,
            envelope(
                host_stream,
                FrameKind::NewAgent,
                seq,
                &protocol::NewAgentPayload {
                    agent_id: AgentId(agent_id.to_owned()),
                    name: "Agent One".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    launch_profile_id: None,
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
                    activity_summary: Default::default(),
                },
            ),
        );
        AgentRef {
            local_host_id: host.clone(),
            agent_id: AgentId(agent_id.to_owned()),
        }
    }

    /// A `CommandError(LoadAgent)` must surface a visible error row (which
    /// retires the spinner) while keeping the pending load latch set so the
    /// auto-load effect does not retry, and leaving `agent_loaded` unset.
    #[wasm_bindgen_test]
    fn command_error_load_agent_keeps_latch_and_surfaces_error() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-load-error");
        let instance_stream = StreamPath("/agent/a-1/inst".to_owned());
        let agent_ref = register_agent(
            &state,
            &host,
            "/host/mobile-load-error",
            0,
            "a-1",
            &instance_stream,
        );

        // The chat view latched the load the moment it sent LoadAgent.
        state.agent_load_requests.update(|m| {
            m.insert(agent_ref.clone());
        });

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-load-error",
                FrameKind::CommandError,
                1,
                &CommandErrorPayload {
                    stream: instance_stream.clone(),
                    request_kind: FrameKind::LoadAgent,
                    setting_target: None,
                    operation: "load_agent".to_owned(),
                    code: protocol::CommandErrorCode::Conflict,
                    message: "agent already attached".to_owned(),
                    fatal: false,
                },
            ),
        );

        assert!(
            state
                .agent_load_requests
                .with_untracked(|m| m.contains(&agent_ref)),
            "load latch must stay set so the auto-load effect does not retry"
        );
        assert!(
            !state
                .agent_loaded
                .with_untracked(|m| m.contains(&agent_ref)),
            "agent_loaded must stay unset — no snapshot ever arrived"
        );
        let msgs = state
            .chat_messages
            .with_untracked(|m| m.get(&agent_ref).cloned())
            .unwrap_or_default();
        assert_eq!(msgs.len(), 1, "a visible error row must be surfaced");
        assert!(
            matches!(msgs[0].message.sender, protocol::MessageSender::Error),
            "error row must be tagged as an error message"
        );
        assert!(
            msgs[0].message.content.contains("agent already attached"),
            "error row must carry the server message: {}",
            msgs[0].message.content
        );
        assert!(
            state
                .command_errors_by_host
                .with_untracked(|m| m.get(&host).cloned())
                .is_none(),
            "a chat-local LoadAgent error must not also raise a host-level banner"
        );

        // A second LoadAgent error (the loop the retained latch guards
        // against would deliver more) appends one more row but never clears
        // the latch — the spinner stays gone and there is no runaway.
        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-load-error",
                FrameKind::CommandError,
                2,
                &CommandErrorPayload {
                    stream: instance_stream.clone(),
                    request_kind: FrameKind::LoadAgent,
                    setting_target: None,
                    operation: "load_agent".to_owned(),
                    code: protocol::CommandErrorCode::Conflict,
                    message: "agent already attached".to_owned(),
                    fatal: false,
                },
            ),
        );
        assert!(
            state
                .agent_load_requests
                .with_untracked(|m| m.contains(&agent_ref)),
            "load latch must remain set across repeated errors"
        );
    }

    /// A `CommandError(FetchSessionHistory)` keeps its existing behavior:
    /// clear the per-agent history loading flag and raise a host-level error,
    /// without pushing a chat error row.
    #[wasm_bindgen_test]
    fn command_error_fetch_session_history_clears_loading_only() {
        let state = AppState::new();
        let host = primed_host(&state, "mobile-history-error");
        let instance_stream = StreamPath("/agent/a-1/inst".to_owned());
        let agent_ref = register_agent(
            &state,
            &host,
            "/host/mobile-history-error",
            0,
            "a-1",
            &instance_stream,
        );

        state.session_history.update(|m| {
            m.insert(
                agent_ref.clone(),
                SessionHistoryState {
                    message_count: 3,
                    oldest_seq: Some(5),
                    has_more_before: true,
                    loading: true,
                },
            );
        });

        dispatch_envelope(
            &state,
            &host,
            envelope(
                "/host/mobile-history-error",
                FrameKind::CommandError,
                1,
                &CommandErrorPayload {
                    stream: instance_stream.clone(),
                    request_kind: FrameKind::FetchSessionHistory,
                    setting_target: None,
                    operation: "fetch_session_history".to_owned(),
                    code: protocol::CommandErrorCode::Internal,
                    message: "history read failed".to_owned(),
                    fatal: false,
                },
            ),
        );

        let loading = state
            .session_history
            .with_untracked(|m| m.get(&agent_ref).map(|h| h.loading));
        assert_eq!(
            loading,
            Some(false),
            "FetchSessionHistory error must clear the history loading flag"
        );
        assert!(
            state
                .chat_messages
                .with_untracked(|m| m.get(&agent_ref).cloned())
                .unwrap_or_default()
                .is_empty(),
            "FetchSessionHistory error must not push a chat error row"
        );
        assert!(
            state
                .command_errors_by_host
                .with_untracked(|m| m.get(&host).cloned())
                .is_some(),
            "FetchSessionHistory error must still raise a host-level error"
        );
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
            token_usage: Some(protocol::MessageTokenUsage::request_known(
                protocol::TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    total_tokens: 10,
                    cached_prompt_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                },
            )),
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
                .and_then(|t| t.request.known_usage())
                .is_some_and(|u| u.total_tokens == 10),
            "token_usage request scope patched"
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
                .and_then(|t| t.request.known_usage())
                .is_some_and(|u| u.total_tokens == 10),
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
                token_usage: Some(protocol::MessageTokenUsage::request_known(
                    protocol::TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        total_tokens: 10,
                        cached_prompt_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                    },
                )),
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
                .and_then(|t| t.request.known_usage())
                .is_some_and(|u| u.total_tokens == 10),
            "token_usage request scope patched in place"
        );
    }

    #[wasm_bindgen_test]
    fn ordered_stream_events_render_without_client_identity_ownership() {
        let state = AppState::new();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId("mobile-server-ordered-stream".to_owned()),
            agent_id: AgentId("agent".to_owned()),
        };
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamStart(protocol::StreamStartData {
                message_id: Some("start-id".to_owned()),
                agent: "codex".to_owned(),
                model: None,
            }),
        );
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
                message_id: Some("delta-id".to_owned()),
                text: "server text".to_owned(),
            }),
        );
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamReasoningDelta(protocol::StreamTextDeltaData {
                message_id: None,
                text: "server reasoning".to_owned(),
            }),
        );

        let preview = state
            .streaming_text
            .with_untracked(|map| map.get(&agent_ref).cloned())
            .expect("server Start owns the live preview");
        assert_eq!(preview.text.get_untracked(), "server text");
        assert_eq!(preview.reasoning.get_untracked(), "server reasoning");

        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamEnd(protocol::StreamEndData {
                message: protocol::ChatMessage {
                    message_id: Some(protocol::ChatMessageId("end-id".to_owned())),
                    timestamp: 1,
                    sender: protocol::MessageSender::Assistant {
                        agent: "codex".to_owned(),
                    },
                    content: "server completion".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }),
        );

        let rows = state
            .chat_messages
            .with_untracked(|map| map.get(&agent_ref).cloned())
            .expect("authoritative completion row");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message.content, "server completion");
        assert_eq!(
            rows[0].message.message_id,
            Some(protocol::ChatMessageId("end-id".to_owned()))
        );
        assert!(matches!(
            rows[0].message.sender,
            protocol::MessageSender::Assistant { .. }
        ));
        assert!(
            state
                .streaming_text
                .with_untracked(|map| !map.contains_key(&agent_ref))
        );
    }

    #[wasm_bindgen_test]
    fn completed_messages_are_not_rewritten_and_repeated_server_rows_match_history() {
        let state = AppState::new();
        let agent_ref = AgentRef {
            local_host_id: LocalHostId("mobile-empty-completion".to_owned()),
            agent_id: AgentId("agent".to_owned()),
        };
        let assistant_message = |id: &str, content: &str| protocol::ChatMessage {
            message_id: Some(protocol::ChatMessageId(id.to_owned())),
            timestamp: 1,
            sender: protocol::MessageSender::Assistant {
                agent: "codex".to_owned(),
            },
            content: content.to_owned(),
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
            ChatEvent::StreamStart(protocol::StreamStartData {
                message_id: Some("empty-item".to_owned()),
                agent: "codex".to_owned(),
                model: None,
            }),
        );
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamEnd(protocol::StreamEndData {
                message: assistant_message("empty-item", ""),
            }),
        );

        let live_empty = state
            .chat_messages
            .with_untracked(|rows| rows.get(&agent_ref).cloned())
            .expect("empty completion row");
        assert_eq!(live_empty.len(), 1);
        assert_eq!(
            live_empty[0].message.message_id,
            Some(protocol::ChatMessageId("empty-item".to_owned()))
        );
        assert!(live_empty[0].message.content.is_empty());

        let mut replay = MobileHistoryReplay::default();
        replay.apply(
            ChatEvent::StreamEnd(protocol::StreamEndData {
                message: assistant_message("empty-history", ""),
            }),
            &agent_ref.local_host_id,
            &agent_ref,
        );
        assert_eq!(replay.rows.len(), 1);
        assert!(replay.rows[0].message.content.is_empty());

        let mut authoritative = assistant_message("authoritative", "");
        authoritative.reasoning = Some(protocol::ReasoningData {
            text: "reasoning only".to_owned(),
            tokens: None,
            signature: None,
            blob: None,
        });
        authoritative.tool_calls = vec![protocol::ToolUseData {
            id: "tool-call".to_owned(),
            name: "tool".to_owned(),
            arguments: serde_json::json!({}),
        }];
        authoritative.images = Some(vec![protocol::ImageData {
            media_type: "image/png".to_owned(),
            data: "image-data".to_owned(),
        }]);
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::StreamEnd(protocol::StreamEndData {
                message: authoritative.clone(),
            }),
        );
        let authoritative_row = state
            .chat_messages
            .with_untracked(|rows| rows.get(&agent_ref).and_then(|rows| rows.last()).cloned())
            .expect("authoritative completion row");
        assert_eq!(
            authoritative_row
                .message
                .reasoning
                .as_ref()
                .map(|value| value.text.as_str()),
            Some("reasoning only")
        );
        assert_eq!(authoritative_row.message.tool_calls.len(), 1);
        assert_eq!(
            authoritative_row.message.images.as_ref().map(Vec::len),
            Some(1)
        );

        let mut authoritative_replay = MobileHistoryReplay::default();
        authoritative_replay.apply(
            ChatEvent::StreamEnd(protocol::StreamEndData {
                message: authoritative,
            }),
            &agent_ref.local_host_id,
            &agent_ref,
        );
        assert_eq!(
            authoritative_replay.rows[0]
                .message
                .reasoning
                .as_ref()
                .map(|value| value.text.as_str()),
            Some("reasoning only")
        );
        assert_eq!(authoritative_replay.rows[0].message.tool_calls.len(), 1);
        assert_eq!(
            authoritative_replay.rows[0]
                .message
                .images
                .as_ref()
                .map(Vec::len),
            Some(1)
        );

        let original = assistant_message("message-added", "original");
        let repeated = assistant_message("message-added", "server second row");
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::MessageAdded(original.clone()),
        );
        apply_chat_event(
            &state,
            &agent_ref,
            ChatEvent::MessageAdded(repeated.clone()),
        );
        let live_rows = state
            .chat_messages
            .with_untracked(|rows| rows.get(&agent_ref).cloned())
            .expect("live rows");
        assert_eq!(
            live_rows
                .iter()
                .filter(|entry| {
                    entry.message.message_id
                        == Some(protocol::ChatMessageId("message-added".to_owned()))
                })
                .count(),
            2,
            "server rows render in order even when they carry the same metadata key"
        );
        assert_eq!(
            live_rows
                .iter()
                .filter(|entry| {
                    entry.message.message_id
                        == Some(protocol::ChatMessageId("message-added".to_owned()))
                })
                .map(|entry| entry.message.content.clone())
                .collect::<Vec<_>>(),
            vec!["original".to_owned(), "server second row".to_owned()]
        );

        let mut replay = MobileHistoryReplay::default();
        replay.apply(
            ChatEvent::MessageAdded(original),
            &agent_ref.local_host_id,
            &agent_ref,
        );
        replay.apply(
            ChatEvent::MessageAdded(repeated),
            &agent_ref.local_host_id,
            &agent_ref,
        );
        assert_eq!(replay.rows.len(), 2);
        assert_eq!(
            replay
                .rows
                .iter()
                .filter(|entry| {
                    entry.message.message_id
                        == Some(protocol::ChatMessageId("message-added".to_owned()))
                })
                .count(),
            2
        );
        assert!(
            !replay
                .rows
                .iter()
                .any(|entry| matches!(entry.message.sender, protocol::MessageSender::Error))
        );
    }
}
