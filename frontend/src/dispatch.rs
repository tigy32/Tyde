use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};

use protocol::{
    AgentActivityStatsPayload, AgentActivitySummaryPayload, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentClosedPayload, AgentErrorPayload, AgentId, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, AgentsViewPreferencesNotifyPayload,
    BackendConfigSchemasPayload, BackendConfigSnapshotsPayload, BackendSetupPayload,
    BrowseBootstrapListing, BrowseBootstrapPayload, ByteRange, ChatEvent,
    CodeIntelDiagnosticsPayload, CodeIntelErrorContext, CodeIntelErrorPayload,
    CodeIntelFileModelPayload, CodeIntelHoverResultPayload, CodeIntelLocation,
    CodeIntelNavigateResultPayload, CodeIntelOverviewPayload, CodeIntelReferenceLine,
    CodeIntelReferencesCompletePayload, CodeIntelReferencesFileResult,
    CodeIntelReferencesResultsPayload, CodeIntelStatusPayload, CodeIntelStatusScope,
    CommandErrorPayload, CustomAgentNotifyPayload, Envelope, FrameKind, HostBootstrapPayload,
    HostBrowseEntriesPayload, HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload,
    LaunchProfileCatalogPayload, McpServerNotifyPayload, MobileAccessStatePayload,
    MobilePairingOfferPayload, MobilePairingState, NewAgentPayload, NewTerminalPayload,
    ProjectBootstrapPayload, ProjectEventPayload, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitCommitResultPayload, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload, ProjectPath,
    ProjectSearchCompletePayload, ProjectSearchResultsPayload, ProtocolValidator,
    QueuedMessagesPayload, RejectCode, RejectPayload, ReviewBootstrapPayload, ReviewCommentSource,
    ReviewErrorContext, ReviewEventPayload, ReviewId, ReviewSuggestionState, SessionHistoryPayload,
    SessionId, SessionListPayload, SessionSchemasPayload, SessionSettingsPayload,
    SkillNotifyPayload, SteeringNotifyPayload, StreamPath, TaskTokenUsagePayload,
    TeamDraftNotifyPayload, TeamMemberBindingNotifyPayload, TeamMemberId, TeamMemberNotifyPayload,
    TeamMemberShuffleSuggestionNotifyPayload, TeamNotifyPayload, TeamPresetCatalogNotifyPayload,
    TerminalBootstrapPayload, TerminalErrorPayload, TerminalExitPayload, TerminalOutputPayload,
    TerminalStartPayload, WelcomePayload, WorkflowNotifyPayload, WorkflowRunNotifyPayload,
};

use crate::line_source::FileLines;
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, ActiveTerminalRef, AgentInfo, AppState, ChatMessageEntry,
    CodeIntelKey, ConnectionStatus, NativeSettingsSaveState, OpenFile, OrchestrationRecord,
    ProjectInfo, ProjectReferencesMode, ProjectReferencesUiState, ReviewActionTarget,
    SessionHistoryState, SessionInfo, StreamingState, StreamingToolRequest, TabContent,
    TerminalInfo, ToolCallId, ToolRequestEntry, TransientEvent, WorkflowPanelError,
    reduce_diff_response, root_display_name, sort_project_infos,
};

struct FrontendSeqValidator {
    expected: HashMap<(String, StreamPath), u64>,
}

impl FrontendSeqValidator {
    fn new() -> Self {
        Self {
            expected: HashMap::new(),
        }
    }

    fn validate(
        &mut self,
        host_id: &str,
        stream: &StreamPath,
        seq: u64,
        kind: FrameKind,
    ) -> Result<(), String> {
        let key = (host_id.to_string(), stream.clone());
        let expected = self.expected.get(&key).copied().unwrap_or(0);
        if seq != expected {
            return Err(format!(
                "sequence mismatch on host {host_id} stream {stream} kind {kind}: expected {expected}, got {seq}"
            ));
        }
        self.expected.insert(key, expected + 1);
        Ok(())
    }

    fn forget_host(&mut self, host_id: &str) {
        self.expected.retain(|(h, _), _| h != host_id);
    }

    fn forget_host_except_stream(&mut self, host_id: &str, stream: &StreamPath) {
        self.expected
            .retain(|(h, s), _| h != host_id || s == stream);
    }
}

struct PerHostProtocolValidators {
    by_host: HashMap<String, ProtocolValidator>,
}

impl PerHostProtocolValidators {
    fn new() -> Self {
        Self {
            by_host: HashMap::new(),
        }
    }

    fn validate(&mut self, host_id: &str, envelope: &Envelope) -> Result<(), String> {
        let validator = self.by_host.entry(host_id.to_string()).or_default();
        validator
            .validate_envelope(envelope)
            .map_err(|error| error.to_string())
    }

    fn reset_host(&mut self, host_id: &str) {
        self.by_host.remove(host_id);
    }
}

/// Drop per-host inbound sequence state only. Tests use this to rewind seq
/// counters while preserving protocol bootstrap state.
pub fn clear_host_seqs(host_id: &str) {
    INBOUND_SEQ.with(|validator| validator.borrow_mut().forget_host(host_id));
}

/// Drop all per-host inbound validator state for a host. Production reconnect
/// paths use this so replayed bootstraps validate as the first frames on the
/// fresh connection.
pub fn reset_inbound_state_for_host(host_id: &str) {
    INBOUND_SEQ.with(|validator| validator.borrow_mut().forget_host(host_id));
    INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().reset_host(host_id));
}

/// Test helper: drop the seq counter for a single `(host_id, stream)`
/// pair so the next envelope on that stream is accepted at seq 0.
#[allow(dead_code)]
pub fn clear_stream_seq_for_tests(host_id: &str, stream: &StreamPath) {
    INBOUND_SEQ.with(|validator| {
        let key = (host_id.to_string(), stream.clone());
        validator.borrow_mut().expected.remove(&key);
    });
}

/// Reset the inbound `ProtocolValidator`. The validator carries per-stream
/// state (registered agent streams, recent-frame history) which persists for
/// the lifetime of the wasm thread. Production code never calls this; it
/// exists so wasm tests, which reuse stream paths (`/agents/a-new`, etc.)
/// across independent test cases, can start each test with a clean validator.
#[allow(dead_code)]
pub fn reset_inbound_protocol() {
    INBOUND_PROTOCOL.with(|validator| *validator.borrow_mut() = PerHostProtocolValidators::new());
}

/// Test helper: prime the inbound validators for `host_id` so subsequent
/// dispatch calls behave as if the server had already delivered a
/// `Welcome` (seq 0) + `HostBootstrap` (seq 1) pair on the
/// `/host/<host_id>` stream.
///
/// After priming, the `ProtocolValidator` considers the host stream to
/// have observed a bootstrap (so non-bootstrap frames will not be
/// rejected as "before HostBootstrap"), but the `FrontendSeqValidator`
/// has been rewound so tests can dispatch their first envelope at seq
/// `0` without a seq-mismatch error.
///
/// The synthetic `HostBootstrap` is empty, so any host-keyed slice the
/// test populated via `AppState` setters survives — the bootstrap only
/// inserts empty maps/vecs for the keyed slots that already accept
/// upserts.
#[allow(dead_code)]
pub fn prime_host_for_tests(state: &AppState, host_id: &str) {
    use protocol::{
        BackendSetupPayload as BootstrapBackendSetup, HostBootstrapPayload as BootstrapHostPayload,
        HostSettings as BootstrapHostSettings, MobileAccessStatePayload as BootstrapMobileAccess,
        MobileBrokerStatus as BootstrapBrokerStatus, MobilePairingState as BootstrapPairingState,
        PROTOCOL_VERSION, TYDE_VERSION, TeamPresetCatalog as BootstrapTeamPresetCatalog,
        WelcomePayload as BootstrapWelcome,
    };

    let host_stream = StreamPath(format!("/host/{host_id}"));

    // Reset both validators so we start from a known state.
    reset_inbound_state_for_host(host_id);

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
    dispatch_envelope(state, host_id, welcome_env);
    let bootstrap_env =
        Envelope::from_payload(host_stream, FrameKind::HostBootstrap, 1, &bootstrap)
            .expect("synthetic HostBootstrap");
    dispatch_envelope(state, host_id, bootstrap_env);

    // Rewind only the FrontendSeqValidator. The ProtocolValidator keeps
    // the saw_welcome/saw_bootstrap state from the synthetic frames, so
    // a follow-up test envelope at seq 0 passes the bootstrap-first
    // check while the seq counter restarts at 0.
    clear_host_seqs(host_id);
}

/// Test helper: dispatch a synthetic `AgentBootstrap` on
/// `instance_stream` so subsequent dispatches on that agent stream pass
/// the bootstrap-first check, and rewind the per-stream seq counter so
/// the test's first envelope can use seq 0.
///
/// Use after seeding the agent into `state.agents` (e.g. via a
/// `NewAgent` dispatch on the host stream). The bootstrap payload
/// includes an `AgentStart` so the validator's `chat_event` checks pass
/// for follow-up `ChatEvent` frames.
#[allow(dead_code)]
pub fn prime_agent_stream_for_tests(
    state: &AppState,
    host_id: &str,
    instance_stream: &StreamPath,
    agent_payload: &protocol::AgentStartPayload,
) {
    use protocol::AgentBootstrapEvent as BootstrapEvent;
    use protocol::AgentBootstrapPayload as BootstrapPayload;

    let bootstrap_env = Envelope::from_payload(
        instance_stream.clone(),
        FrameKind::AgentBootstrap,
        0,
        &BootstrapPayload {
            events: vec![BootstrapEvent::AgentStart(agent_payload.clone())],
            latest_output: Default::default(),
        },
    )
    .expect("synthetic AgentBootstrap");
    dispatch_envelope(state, host_id, bootstrap_env);

    clear_stream_seq_for_tests(host_id, instance_stream);
}

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
    static INBOUND_PROTOCOL: RefCell<PerHostProtocolValidators> =
        RefCell::new(PerHostProtocolValidators::new());
}

fn report_dispatch_error(
    _state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    kind: FrameKind,
    message: impl Into<String>,
) {
    let message = message.into();
    log::error!(
        "frontend dispatch error host={} stream={} kind={}: {}",
        host_id,
        stream,
        kind,
        message
    );
}

pub(crate) fn should_auto_force_upgrade(
    code: RejectCode,
    is_managed: bool,
    already_attempted: bool,
) -> bool {
    matches!(code, RejectCode::IncompatibleProtocol) && is_managed && !already_attempted
}

pub fn dispatch_envelope(state: &AppState, host_id: &str, envelope: Envelope) {
    if let Err(error) = INBOUND_SEQ.with(|validator| {
        validator
            .borrow_mut()
            .validate(host_id, &envelope.stream, envelope.seq, envelope.kind)
    }) {
        // A sequence mismatch means a frame was lost or the stream desynced.
        // The validator does not advance `expected` on mismatch, so every
        // later frame on this stream would now also mismatch and be silently
        // dropped — the connection is permanently wedged until reconnect.
        // Surface it as a connection error so the user can see the host is
        // broken and reconnect (which resets seq=0 on both sides) rather than
        // staring at a "connected" host that silently swallows every reply.
        let status_message = format!("stream desync — reconnect required: {error}");
        report_dispatch_error(state, host_id, &envelope.stream, envelope.kind, error);
        state.connection_statuses.update(|statuses| {
            statuses.insert(host_id.to_string(), ConnectionStatus::Error(status_message));
        });
        return;
    }
    if envelope.kind == FrameKind::Welcome {
        if let Err(error) = envelope.parse_payload::<WelcomePayload>() {
            report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse Welcome payload: {error}"),
            );
            return;
        }
        INBOUND_SEQ.with(|validator| {
            validator
                .borrow_mut()
                .forget_host_except_stream(host_id, &envelope.stream);
        });
        INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().reset_host(host_id));
    }
    if let Err(error) =
        INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().validate(host_id, &envelope))
    {
        report_dispatch_error(
            state,
            host_id,
            &envelope.stream,
            envelope.kind,
            format!("protocol violation: {error}"),
        );
        return;
    }

    match envelope.kind {
        FrameKind::Welcome => {
            state.command_errors_by_host.update(|errors| {
                errors.remove(host_id);
            });
            // Handshake succeeded, so the connection lifecycle is healthy again:
            // release the one-shot forced-upgrade guard so a future
            // incompatibility (e.g. after the app itself updates) can trigger
            // one fresh auto-upgrade attempt.
            state.clear_upgrade_attempted(host_id);
            state.connection_statuses.update(|statuses| {
                statuses.insert(host_id.to_string(), ConnectionStatus::Connected);
            });
            // Sessions/projects/teams etc. now arrive via HostBootstrap
            // (seq 1 on the host stream) — see the HostBootstrap arm below.
            log::info!("connected to host {}", host_id);
        }
        FrameKind::HostBootstrap => match envelope.parse_payload::<HostBootstrapPayload>() {
            Ok(payload) => apply_host_bootstrap(state, host_id, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_bootstrap payload: {error}"),
            ),
        },
        FrameKind::AgentBootstrap => match envelope.parse_payload::<AgentBootstrapPayload>() {
            Ok(payload) => apply_agent_bootstrap(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_bootstrap payload: {error}"),
            ),
        },
        FrameKind::ProjectBootstrap => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_bootstrap on malformed project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectBootstrapPayload>() {
                Ok(payload) => apply_project_bootstrap(state, host_id, project_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_bootstrap payload: {error}"),
                ),
            }
        }
        FrameKind::ReviewBootstrap => {
            let Some(review_id) = resolve_review_id(&envelope.stream) else {
                log::warn!("review_bootstrap on non-review stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<ReviewBootstrapPayload>() {
                Ok(payload) => apply_review_bootstrap(state, &review_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse review_bootstrap payload: {error}"),
                ),
            }
        }
        FrameKind::BrowseBootstrap => match envelope.parse_payload::<BrowseBootstrapPayload>() {
            Ok(payload) => apply_browse_bootstrap(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse browse_bootstrap payload: {error}"),
            ),
        },
        FrameKind::TerminalBootstrap => {
            match envelope.parse_payload::<TerminalBootstrapPayload>() {
                Ok(payload) => apply_terminal_bootstrap(state, host_id, &envelope.stream, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse terminal_bootstrap payload: {error}"),
                ),
            }
        }
        FrameKind::Reject => match envelope.parse_payload::<RejectPayload>() {
            Ok(payload) => {
                log::error!(
                    "connection rejected on host {}: {}",
                    host_id,
                    payload.message
                );
                // Phase 2 safety net: a MANAGED remote that rejects with
                // IncompatibleProtocol — after Phase 1's normal connect path
                // already served/launched a release it judged compatible — gets
                // exactly ONE forced upgrade-and-reconnect. The forced path
                // (`force_upgrade_managed_host`) bypasses the "serve as-is"
                // decision so it won't no-op when the running version matches but
                // the protocol is still incompatible. The per-host
                // `upgrade_attempted` guard makes this strictly one-shot: no
                // fallback, no loop. Any other case (already attempted,
                // non-managed host, or InvalidHandshake) keeps the existing
                // terminal-error behavior.
                let eligible = should_auto_force_upgrade(
                    payload.code,
                    crate::app::is_managed_remote_host(state, host_id),
                    state.upgrade_already_attempted(host_id),
                );
                if eligible {
                    state.mark_upgrade_attempted(host_id);
                    // The connection is still being established as far as the
                    // user is concerned — keep it in Connecting while the forced
                    // upgrade + reconnect runs.
                    state.connection_statuses.update(|statuses| {
                        statuses.insert(host_id.to_string(), ConnectionStatus::Connecting);
                    });
                    let state = state.clone();
                    let host_id = host_id.to_string();
                    let reject_message = payload.message;
                    wasm_bindgen_futures::spawn_local(async move {
                        match crate::bridge::force_upgrade_managed_host(host_id.clone()).await {
                            Ok(snapshot) => {
                                state.host_lifecycle_statuses.update(|statuses| {
                                    statuses.insert(
                                        host_id.clone(),
                                        crate::bridge::RemoteHostLifecycleStatus::Snapshot {
                                            snapshot,
                                        },
                                    );
                                });
                                crate::app::connect_one_host(state, host_id).await;
                            }
                            Err(error) => {
                                log::error!(
                                    "forced upgrade after IncompatibleProtocol failed for host {host_id}: {error}"
                                );
                                state.connection_statuses.update(|statuses| {
                                    statuses.insert(
                                        host_id,
                                        ConnectionStatus::Error(format!(
                                            "incompatible protocol ({reject_message}); forced upgrade failed: {error}"
                                        )),
                                    );
                                });
                            }
                        }
                    });
                } else {
                    state.connection_statuses.update(|statuses| {
                        statuses.insert(
                            host_id.to_string(),
                            ConnectionStatus::Error(payload.message),
                        );
                    });
                    clear_session_history_loading_for_host(state, host_id);
                }
            }
            Err(error) => {
                report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse reject payload: {error}"),
                );
            }
        },
        FrameKind::CommandError => match envelope.parse_payload::<CommandErrorPayload>() {
            Ok(payload) => {
                let message = format!(
                    "{} failed on {}: {}",
                    payload.operation, payload.stream, payload.message
                );
                log::error!(
                    "command error host={} request_kind={} operation={} request_stream={} code={:?}: {}",
                    host_id,
                    payload.request_kind,
                    payload.operation,
                    payload.stream,
                    payload.code,
                    payload.message
                );
                state.command_errors_by_host.update(|errors| {
                    errors.insert(host_id.to_string(), message);
                });
                clear_session_history_loading_on_error(state, host_id, &payload);
                // A backend-native settings save is a `SetSetting` whose result
                // only lands via a refreshed native snapshot. If the server
                // rejects it (e.g. Tycode SaveSettings fails), no snapshot is
                // emitted, so an in-flight `Pending` save would leave the native
                // settings page stuck in "Saving…" forever. Flip it to `Failed`
                // so controls unlock and the server's reason is shown.
                fail_native_settings_pending_on_error(state, host_id, &payload);
                // Release any review-side pending gate the rejected
                // command was holding. Without this, a server-side
                // failure (unknown project, git error, malformed
                // payload) would leave the "Review changes" or
                // Submit/Cancel buttons disabled forever — the only
                // other clear path is `ReviewListChanged` /
                // `ReviewEvent::Snapshot`, neither of which fires for
                // a rejected request.
                clear_review_pending_on_error(state, host_id, &payload);
                // CommandError carries no parent/branch correlation — its
                // `stream` is the host stream the request was sent on — so
                // when creates under *different* parents run concurrently
                // (the server lock is only per parent) we cannot tell which
                // one failed. Best effort: mark the oldest in-flight entry
                // for this host with the message so the create modal can
                // surface it inline. Entries are additionally time-bounded
                // (PENDING_WORKBENCH_CREATE_TTL_MS) so a mis-correlated or
                // unconsumed entry cannot linger and cause a spurious
                // active-project switch later.
                if matches!(payload.request_kind, FrameKind::WorkbenchCreate) {
                    let now = crate::state::now_ms();
                    state.pending_workbench_creates.update(|pending| {
                        pending.retain(|p| !p.is_stale(now));
                        if let Some(entry) = pending
                            .iter_mut()
                            .find(|p| p.host_id == host_id && p.error.is_none())
                        {
                            entry.error = Some(payload.message.clone());
                        }
                    });
                }
                // Surface workflow command failures inline in the Workflows
                // panel instead of only logging them. `CommandErrorPayload`
                // carries no `workflow_id`, so this is a panel-level banner; it
                // clears on the next successful notify for the failed operation.
                if matches!(
                    payload.request_kind,
                    FrameKind::WorkflowRefresh
                        | FrameKind::TriggerWorkflow
                        | FrameKind::CancelWorkflow
                ) {
                    state.workflow_command_errors.update(|errors| {
                        errors.insert(
                            host_id.to_string(),
                            WorkflowPanelError {
                                request_kind: payload.request_kind,
                                message: payload.message.clone(),
                            },
                        );
                    });
                }
            }
            Err(error) => {
                report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse command_error payload: {error}"),
                );
            }
        },
        FrameKind::HostSettings => match envelope.parse_payload::<HostSettingsPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch host_settings host={} enabled_backends={} default_backend={:?} debug_mcp={} agent_control_mcp={}",
                    host_id,
                    payload.settings.enabled_backends.len(),
                    payload.settings.default_backend,
                    payload.settings.tyde_debug_mcp_enabled,
                    payload.settings.tyde_agent_control_mcp_enabled
                );
                state.host_settings_by_host.update(|settings| {
                    settings.insert(host_id.to_string(), payload.settings);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_settings payload: {error}"),
            ),
        },
        FrameKind::AgentActivitySummary => {
            match envelope.parse_payload::<AgentActivitySummaryPayload>() {
                Ok(payload) => apply_agent_activity_summary(state, host_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse agent_activity_summary payload: {error}"),
                ),
            }
        }
        FrameKind::TaskTokenUsage => match envelope.parse_payload::<TaskTokenUsagePayload>() {
            Ok(payload) => apply_task_token_usage(state, host_id, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse task_token_usage payload: {error}"),
            ),
        },
        FrameKind::AgentActivityStats => {
            match envelope.parse_payload::<AgentActivityStatsPayload>() {
                Ok(payload) => apply_agent_activity_stats(state, host_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse agent_activity_stats payload: {error}"),
                ),
            }
        }
        FrameKind::AgentsViewPreferencesNotify => {
            match envelope.parse_payload::<AgentsViewPreferencesNotifyPayload>() {
                Ok(payload) => {
                    log::info!(
                        "dispatch agents_view_preferences_notify host={} load_error={}",
                        host_id,
                        payload.snapshot.load_error.is_some()
                    );
                    state.apply_agents_view_snapshot(host_id, payload.snapshot);
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse agents_view_preferences_notify payload: {error}"),
                ),
            }
        }
        FrameKind::MobileAccessState => {
            match envelope.parse_payload::<MobileAccessStatePayload>() {
                Ok(payload) => apply_mobile_access_state(state, host_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse mobile_access_state payload: {error}"),
                ),
            }
        }
        FrameKind::MobilePairingOffer => {
            match envelope.parse_payload::<MobilePairingOfferPayload>() {
                Ok(payload) => {
                    // Avoid logging the qr_uri itself — it embeds a
                    // pre-shared key the mobile app uses to derive the
                    // session keys. The offer_id and expiry are enough
                    // for forensic logs.
                    log::info!(
                        "dispatch mobile_pairing_offer host={} offer_id={} expires_at_ms={}",
                        host_id,
                        payload.offer_id,
                        payload.expires_at_ms
                    );
                    state.mobile_pairing_start_pending.update(|set| {
                        set.remove(host_id);
                    });
                    state.mobile_pairing_offer.update(|m| {
                        m.insert(host_id.to_string(), payload);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse mobile_pairing_offer payload: {error}"),
                ),
            }
        }
        FrameKind::BackendSetup => match envelope.parse_payload::<BackendSetupPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch backend_setup host={} backends={}",
                    host_id,
                    payload.backends.len()
                );
                state.backend_setup_by_host.update(|setup| {
                    setup.insert(host_id.to_string(), payload.backends);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse backend_setup payload: {error}"),
            ),
        },
        FrameKind::SessionSchemas => match envelope.parse_payload::<SessionSchemasPayload>() {
            Ok(payload) => {
                state.session_schemas.update(|schemas_by_host| {
                    let host_schemas = schemas_by_host.entry(host_id.to_string()).or_default();
                    host_schemas.clear();
                    for schema in payload.schemas {
                        host_schemas.insert(schema.backend_kind(), schema);
                    }
                });
                state.schemas_loaded_for_host.update(|loaded| {
                    loaded.insert(host_id.to_string(), true);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse session_schemas payload: {error}"),
            ),
        },
        FrameKind::BackendConfigSchemas => {
            match envelope.parse_payload::<BackendConfigSchemasPayload>() {
                Ok(payload) => {
                    state.backend_config_schemas.update(|schemas_by_host| {
                        let host_schemas = schemas_by_host.entry(host_id.to_string()).or_default();
                        host_schemas.clear();
                        for schema in payload.schemas {
                            host_schemas.insert(schema.backend_kind, schema);
                        }
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse backend_config_schemas payload: {error}"),
                ),
            }
        }
        FrameKind::BackendConfigSnapshots => {
            match envelope.parse_payload::<BackendConfigSnapshotsPayload>() {
                Ok(payload) => {
                    state.backend_config_snapshots.update(|snapshots_by_host| {
                        let host_snapshots =
                            snapshots_by_host.entry(host_id.to_string()).or_default();
                        host_snapshots.clear();
                        for snapshot in payload.snapshots {
                            host_snapshots.insert(snapshot.backend_kind, snapshot);
                        }
                    });
                    // The server force-emits a native-settings snapshot after
                    // every native save (bypassing dedup), so a frame always
                    // arrives even when the saved document is unchanged (an
                    // accepted no-op or a canonicalize-to-current). Any in-flight
                    // native save for a backend in this frame is therefore
                    // complete — clear its pending gate so the settings page
                    // unlocks instead of sitting in "Saving…" waiting for a
                    // settings-value change that will never come.
                    let native_backends: Vec<_> = payload
                        .native_settings
                        .iter()
                        .map(|snapshot| snapshot.backend_kind)
                        .collect();
                    state.backend_native_settings.update(|snapshots_by_host| {
                        let host_snapshots =
                            snapshots_by_host.entry(host_id.to_string()).or_default();
                        host_snapshots.clear();
                        for snapshot in payload.native_settings {
                            host_snapshots.insert(snapshot.backend_kind, snapshot);
                        }
                    });
                    if !native_backends.is_empty() {
                        state.native_settings_save_state.update(|states_by_host| {
                            if let Some(by_kind) = states_by_host.get_mut(host_id) {
                                for kind in &native_backends {
                                    by_kind.remove(kind);
                                }
                                if by_kind.is_empty() {
                                    states_by_host.remove(host_id);
                                }
                            }
                        });
                    }
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse backend_config_snapshots payload: {error}"),
                ),
            }
        }
        FrameKind::SessionSettings => {
            let Some(agent_id) = resolve_agent_id(state, host_id, &envelope.stream) else {
                log::warn!("session_settings on unknown stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<SessionSettingsPayload>() {
                Ok(payload) => {
                    state.agent_session_settings.update(|map| {
                        map.insert(agent_id, payload.values);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse session_settings payload: {error}"),
                ),
            }
        }
        FrameKind::QueuedMessages => {
            let Some(agent_id) = resolve_agent_id(state, host_id, &envelope.stream) else {
                log::warn!("queued_messages on unknown stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<QueuedMessagesPayload>() {
                Ok(payload) => {
                    log::info!(
                        "dispatch queued_messages host={} agent_id={} count={}",
                        host_id,
                        agent_id,
                        payload.messages.len()
                    );
                    state.agent_message_queue.update(|map| {
                        map.insert(agent_id, payload.messages);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse queued_messages payload: {error}"),
                ),
            }
        }
        FrameKind::NewAgent => match envelope.parse_payload::<NewAgentPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch new_agent host={} agent_id={} name={} backend={:?} instance_stream={}",
                    host_id,
                    payload.agent_id,
                    payload.name,
                    payload.backend_kind,
                    payload.instance_stream
                );
                let agent_id = payload.agent_id.clone();
                let origin = payload.origin;
                let team_member_id = payload.team_member_id.clone();
                // Snapshot the fingerprint inputs before moving payload
                // into AgentInfo. The compaction replacement-detection
                // path below reads these to correlate against in-flight
                // old agents' fingerprints captured at start time.
                let fp_project_id = payload.project_id.clone();
                let fp_custom_agent_id = payload.custom_agent_id.clone();
                let fp_backend_kind = payload.backend_kind;
                let info = AgentInfo {
                    host_id: host_id.to_string(),
                    agent_id: payload.agent_id,
                    name: payload.name,
                    origin,
                    backend_kind: payload.backend_kind,
                    workspace_roots: payload.workspace_roots,
                    project_id: payload.project_id,
                    parent_agent_id: payload.parent_agent_id,
                    session_id: payload.session_id,
                    custom_agent_id: payload.custom_agent_id,
                    workflow: payload.workflow,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    started: false,
                    fatal_error: None,
                    activity_summary: payload.activity_summary,
                };
                let project_id = info.project_id.clone();
                let agent_name_for_upgrade = info.name.clone();
                // User-origin and SideQuestion (BTW) agents auto-open a chat
                // tab and steal focus — a side question is something the user
                // just asked for, so it should surface like a user agent.
                // AgentControl and BackendNative agents appear in the sidebar
                // but must not disrupt the user's current view.
                let is_programmatic =
                    !matches!(origin, AgentOrigin::User | AgentOrigin::SideQuestion);
                state.agents.update(|agents| {
                    agents
                        .retain(|agent| !(agent.host_id == host_id && agent.agent_id == agent_id));
                    agents.push(info);
                });

                // If a compaction `Completed` notify arrived before this
                // `NewAgent` echo, the dispatch handler for the notify
                // stashed `(host, new) -> old` so we could flush the
                // retarget here, once the replacement is in `state.agents`.
                let pending_old = state.compaction_pending_completion.with_untracked(|map| {
                    map.get(&(host_id.to_string(), agent_id.clone())).cloned()
                });
                if let Some(old_agent_id) = pending_old {
                    let new_info = state.agents.with_untracked(|agents| {
                        agents
                            .iter()
                            .find(|a| a.host_id == host_id && a.agent_id == agent_id)
                            .cloned()
                    });
                    if let Some(new_info) = new_info {
                        state.finish_compaction_success(&old_agent_id, &new_info);
                    }
                    state.compaction_pending_completion.update(|map| {
                        map.remove(&(host_id.to_string(), agent_id.clone()));
                    });
                    // Completed-early ordering: the retarget already
                    // happened. Skip the auto-tab-open below — the
                    // user's existing chat tab is already retargeted to
                    // this agent.
                    return;
                }

                // Current server contract: `NewAgent` for the
                // replacement arrives BEFORE `Completed` (which in
                // turn arrives before `AgentClosed` for the old).
                // Correlate via the fingerprint we captured at
                // compaction-start time. If this is the replacement,
                // the user's existing chat tab is still pointing at
                // the (alive) old agent — `Completed` will retarget
                // it when it lands. Skip the auto-tab-open path here
                // so the replacement does not steal focus into a
                // duplicate tab.
                let likely_replacement = state.find_compaction_replacement(
                    host_id,
                    team_member_id.as_ref(),
                    fp_project_id.as_ref(),
                    fp_custom_agent_id.as_ref(),
                    fp_backend_kind,
                );
                if let Some(old_agent_id) = likely_replacement {
                    log::info!(
                        "dispatch new_agent host={} agent_id={} recognized as compaction replacement for old={}; deferring tab retarget to Completed",
                        host_id,
                        agent_id,
                        old_agent_id.0,
                    );
                    return;
                }

                // Team-member upgrade: a `pending_team_member` chat tab was
                // opened when the user clicked the team or a report row and
                // is waiting for this spawn echo. Match against
                // `team_member_id` rather than the host/origin so the
                // upgrade works for both User-initiated (via
                // `TeamMemberActivate`) and manager-initiated (via
                // `tyde_team_message_member`) team-member spawns that
                // happen to coincide with a draft tab.
                if let Some(team_member_id) = team_member_id.clone() {
                    let upgraded = upgrade_pending_team_member_tab(
                        state,
                        host_id,
                        &team_member_id,
                        &ActiveAgentRef {
                            host_id: host_id.to_string(),
                            agent_id: agent_id.clone(),
                        },
                        &agent_name_for_upgrade,
                    );
                    if upgraded {
                        return;
                    }
                }

                if is_programmatic {
                    return;
                }

                let target_project =
                    project_id
                        .as_ref()
                        .map(|pid| crate::state::ActiveProjectRef {
                            host_id: host_id.to_string(),
                            project_id: pid.clone(),
                        });
                let active_project = state.active_project.get_untracked();
                let new_active_agent = ActiveAgentRef {
                    host_id: host_id.to_string(),
                    agent_id,
                };

                let agent_name = state
                    .agents
                    .with_untracked(|agents| {
                        agents
                            .iter()
                            .find(|a| {
                                a.host_id == host_id && a.agent_id == new_active_agent.agent_id
                            })
                            .map(|a| a.name.clone())
                    })
                    .unwrap_or_else(|| "Chat".to_string());

                if target_project == active_project {
                    // active_agent is now a Memo over center_zone — the update
                    // below drives it.
                    state.center_zone.update(|cz| {
                        let new_chat = TabContent::empty_chat();
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::chat_with_agent(new_active_agent.clone());
                            tab.label = agent_name.clone();
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::chat_with_agent(new_active_agent.clone()),
                                agent_name.clone(),
                                true,
                            );
                        }
                    });
                } else if let Some(target) = target_project {
                    // Spawned for a project the user isn't currently viewing.
                    // Stash into that project's memory so switching over shows it.
                    state.project_view_memory.update(|map| {
                        let slot = map.entry(target).or_default();
                        let cz = slot.center_zone.get_or_insert_with(Default::default);
                        let new_chat = TabContent::empty_chat();
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::chat_with_agent(new_active_agent);
                            tab.label = agent_name;
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::chat_with_agent(new_active_agent),
                                agent_name,
                                true,
                            );
                        }
                    });
                } else {
                    // No project context — fall through to global behavior.
                    // active_agent is a Memo over center_zone; the update below
                    // drives it.
                    state.center_zone.update(|cz| {
                        let new_chat = TabContent::empty_chat();
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::chat_with_agent(new_active_agent.clone());
                            tab.label = agent_name.clone();
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::chat_with_agent(new_active_agent),
                                agent_name,
                                true,
                            );
                        }
                    });
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse new_agent payload: {error}"),
            ),
        },
        FrameKind::AgentStart => match envelope.parse_payload::<AgentStartPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch agent_start host={} agent_id={} name={} backend={:?}",
                    host_id,
                    payload.agent_id,
                    payload.name,
                    payload.backend_kind
                );
                apply_agent_started(state, host_id, &payload.agent_id, payload.session_id);
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_start payload: {error}"),
            ),
        },
        FrameKind::AgentRenamed => match envelope.parse_payload::<AgentRenamedPayload>() {
            Ok(payload) => apply_agent_rename(state, host_id, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_renamed payload: {error}"),
            ),
        },
        FrameKind::AgentClosed => match envelope.parse_payload::<AgentClosedPayload>() {
            Ok(payload) => apply_agent_closed(state, host_id, payload.agent_id),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_closed payload: {error}"),
            ),
        },
        FrameKind::AgentCompactNotify => {
            match envelope.parse_payload::<protocol::types::AgentCompactNotifyPayload>() {
                Ok(payload) => apply_agent_compact_notify(state, host_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse agent_compact_notify payload: {error}"),
                ),
            }
        }
        FrameKind::TeamCompactNotify => {
            match envelope.parse_payload::<protocol::types::TeamCompactNotifyPayload>() {
                Ok(payload) => apply_team_compact_notify(state, host_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse team_compact_notify payload: {error}"),
                ),
            }
        }
        FrameKind::AgentError => match envelope.parse_payload::<AgentErrorPayload>() {
            Ok(payload) => {
                log::error!(
                    "dispatch agent_error host={} agent_id={} fatal={} code={:?} message={}",
                    host_id,
                    payload.agent_id,
                    payload.fatal,
                    payload.code,
                    payload.message
                );
                let error_agent_id = payload.agent_id.clone();
                if payload.fatal {
                    state.agents.update(|agents| {
                        if let Some(agent) = agents.iter_mut().find(|agent| {
                            agent.host_id == host_id && agent.agent_id == payload.agent_id
                        }) {
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
                state.push_chat_entry(error_agent_id, entry);
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_error payload: {error}"),
            ),
        },
        FrameKind::ChatEvent => dispatch_chat_event(state, host_id, &envelope.stream, &envelope),
        FrameKind::SessionHistory => match envelope.parse_payload::<SessionHistoryPayload>() {
            Ok(payload) => apply_session_history(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse session_history payload: {error}"),
            ),
        },
        FrameKind::SessionList => match envelope.parse_payload::<SessionListPayload>() {
            Ok(payload) => {
                state.sessions.update(|sessions| {
                    sessions.retain(|session| session.host_id != host_id);
                    sessions.extend(payload.sessions.into_iter().map(|summary| SessionInfo {
                        host_id: host_id.to_string(),
                        summary,
                    }));
                    sessions
                        .sort_by_key(|session| std::cmp::Reverse(session.summary.updated_at_ms));
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse session_list payload: {error}"),
            ),
        },
        FrameKind::ProjectEvent => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_notify on malformed project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectEventPayload>() {
                Ok(ProjectEventPayload::ReviewListChanged { reviews }) => {
                    let prev_ids: HashSet<ReviewId> =
                        state.review_summaries.with_untracked(|map| {
                            map.get(&project_id)
                                .map(|s| s.iter().map(|s| s.id.clone()).collect())
                                .unwrap_or_default()
                        });
                    let new_ids: Vec<ReviewId> = reviews
                        .iter()
                        .filter(|s| !prev_ids.contains(&s.id))
                        .map(|s| s.id.clone())
                        .collect();
                    // A successful get-or-create `ReviewCreate` always leaves a
                    // Draft in the project's review list. Release the pending
                    // token whenever this echo shows a draft is present, not
                    // only when it carries a *new* id: a `ProjectBootstrap`
                    // (reconnect / re-subscribe) can fold the existing draft
                    // summary into `review_summaries` before this echo lands,
                    // leaving `new_ids` empty even though the create the user
                    // fired is exactly why a draft now exists. Without this the
                    // "Review changes" button would wedge forever — a
                    // successful create emits no `CommandError` to fall back on.
                    let list_has_draft = reviews
                        .iter()
                        .any(|s| matches!(s.status, protocol::ReviewStatus::Draft));
                    state.review_summaries.update(|map| {
                        map.insert(project_id.clone(), reviews);
                    });
                    let pending_key = (host_id.to_string(), project_id);
                    let has_pending = state
                        .review_create_pending
                        .with_untracked(|m| m.get(&pending_key).copied().unwrap_or(0))
                        > 0;
                    if has_pending && (!new_ids.is_empty() || list_has_draft) {
                        // Pair the most recent new review with one pending
                        // create token and release it. We deliberately do
                        // NOT open a standalone `TabContent::Review`
                        // workbench here: reviews are now integrated into
                        // the normal diff surfaces. The click handler that
                        // started the create (git panel hub / diff-tab
                        // banner) is responsible for the normal changed-file
                        // diff tab; once this draft lands, that diff tab's
                        // review decorations resolve against it.
                        state.review_create_pending.update(|map| {
                            if let Some(count) = map.get_mut(&pending_key) {
                                *count = count.saturating_sub(1);
                                if *count == 0 {
                                    map.remove(&pending_key);
                                }
                            }
                        });
                    }
                }
                Ok(ProjectEventPayload::FilesChanged { files }) => {
                    // A watched change advanced these files' versions. Re-read
                    // any the user has open whose version moved past what we
                    // hold, so the rendered version — and the version stamped on
                    // code-intel queries — catches up to the server's instead of
                    // freezing at open time. Skip files that aren't open, didn't
                    // actually advance, or already have a refresh in flight (the
                    // marker clears when its contents arrive, so a later bump
                    // re-triggers) — that bounds it to one outstanding re-read
                    // per file during a burst of writes.
                    for change in files {
                        let needs_refresh = state.open_files.with_untracked(|open| {
                            open.get(&change.path)
                                .is_some_and(|file| change.version > file.version)
                        });
                        let already_pending = state
                            .pending_file_refreshes
                            .with_untracked(|pending| pending.contains(&change.path));
                        if needs_refresh && !already_pending {
                            crate::actions::refresh_open_file(
                                state,
                                host_id.to_owned(),
                                project_id.clone(),
                                change.path,
                            );
                        }
                    }
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_event payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectNotify => match envelope.parse_payload::<ProjectNotifyPayload>() {
            Ok(ProjectNotifyPayload::Upsert { project }) => {
                // Correlate with any in-flight WorkbenchCreate before mutating
                // state. §3.3 says the matching upsert is the one to switch
                // to; identify it by (parent_project_id, branch) and then
                // drop the pending entry so a future failure doesn't try to
                // clear it.
                let workbench_match = match &project.source {
                    protocol::ProjectSource::GitWorkbench {
                        parent_project_id,
                        branch,
                        ..
                    } => {
                        let mut matched = None;
                        let now = crate::state::now_ms();
                        state.pending_workbench_creates.update(|pending| {
                            // Stale or already-failed entries must not steal
                            // this upsert: only a live in-flight create gets
                            // the auto-switch.
                            pending.retain(|p| !p.is_stale(now));
                            if let Some(idx) = pending.iter().position(|p| {
                                p.host_id == host_id
                                    && &p.parent_project_id == parent_project_id
                                    && &p.branch == branch
                                    && p.error.is_none()
                            }) {
                                pending.remove(idx);
                                matched = Some(project.id.clone());
                            }
                        });
                        matched
                    }
                    protocol::ProjectSource::Standalone { .. } => None,
                };

                state.projects.update(|projects| {
                    if let Some(existing) = projects
                        .iter_mut()
                        .find(|entry| entry.host_id == host_id && entry.project.id == project.id)
                    {
                        existing.project = project;
                    } else {
                        projects.push(ProjectInfo {
                            host_id: host_id.to_string(),
                            project,
                        });
                    }
                    sort_project_infos(projects);
                });

                if let Some(new_id) = workbench_match {
                    state.switch_active_project(Some(crate::state::ActiveProjectRef {
                        host_id: host_id.to_string(),
                        project_id: new_id,
                    }));
                }
            }
            Ok(ProjectNotifyPayload::Delete { project }) => {
                handle_project_delete(state, host_id, &project);
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse project_notify payload: {error}"),
            ),
        },
        FrameKind::ProjectFileList => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_file_list on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectFileListPayload>() {
                Ok(payload) => {
                    state.file_tree.update(|file_tree| {
                        apply_project_file_list(file_tree, project_id, payload);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_file_list payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectGitStatus => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_git_status on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectGitStatusPayload>() {
                Ok(payload) => {
                    state.git_status.update(|git_status| {
                        git_status.insert(project_id, payload.roots);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_git_status payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelOverview => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_overview on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelOverviewPayload>() {
                Ok(payload) => {
                    state.code_intel_overview.update(|overview| {
                        overview.insert(
                            ActiveProjectRef {
                                host_id: host_id.to_owned(),
                                project_id,
                            },
                            payload,
                        );
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_overview payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectGitDiff => match envelope.parse_payload::<ProjectGitDiffPayload>() {
            Ok(payload) => {
                // `diff_contents` is keyed by the explicit owning project
                // identity (this connection's host + the project the response
                // came in on) plus (root, scope, path). The identity is
                // essential: two projects/hosts can share a root path string,
                // and keying on path alone would let one's response overwrite
                // the other's tab. A `None` path is the whole-root all-files
                // review surface, keyed by the empty string (the convention
                // `DiffView` uses to render all files).
                let Some(project_id) = resolve_project_id(&envelope.stream) else {
                    log::debug!(
                        "ignoring ProjectGitDiff on non-project stream {}",
                        envelope.stream
                    );
                    return;
                };
                let payload_path = payload.path.clone().unwrap_or_default();
                let key = crate::state::DiffKey::new(
                    host_id,
                    project_id,
                    payload.root.clone(),
                    payload.scope,
                    payload_path.clone(),
                );
                let perf_key = format!("diff:{}:{payload_path}", payload.root.0);
                let total_lines: usize = payload
                    .files
                    .iter()
                    .flat_map(|f| f.hunks.iter())
                    .map(|h| h.lines.len())
                    .sum();
                crate::perf::log_phase(
                    "diff_open",
                    "response",
                    &perf_key,
                    &format!(
                        " files={} lines={total_lines} mode={:?}",
                        payload.files.len(),
                        payload.context_mode,
                    ),
                );
                let current = state
                    .diff_contents
                    .with_untracked(|diffs| diffs.get(&key).cloned());
                match reduce_diff_response(current.as_ref(), payload) {
                    Some(next) => {
                        state.diff_contents.update(|diffs| {
                            diffs.insert(key, next);
                        });
                    }
                    None => {
                        log::debug!(
                            "ignoring stale/unmatched ProjectGitDiff payload for {:?}",
                            key,
                        );
                    }
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse project_git_diff payload: {error}"),
            ),
        },
        FrameKind::ProjectGitCommitResult => {
            match envelope.parse_payload::<ProjectGitCommitResultPayload>() {
                Ok(payload) => {
                    log::info!(
                        "commit created on root {}: {}",
                        payload.root,
                        payload.commit_hash
                    );
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_git_commit_result payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectFileContents => {
            match envelope.parse_payload::<ProjectFileContentsPayload>() {
                Ok(payload) => {
                    let path = payload.path.clone();
                    let perf_key = format!("file:{}", path.relative_path);
                    let bytes = payload.contents.as_ref().map(|s| s.len()).unwrap_or(0);
                    crate::perf::log_phase(
                        "file_open",
                        "response",
                        &perf_key,
                        &format!(" bytes={bytes} binary={}", payload.is_binary),
                    );
                    let base_label = path
                        .relative_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&path.relative_path)
                        .to_string();
                    let multi_root = state
                        .active_project_info_untracked()
                        .is_some_and(|project| project.project.root_paths().len() > 1);
                    let label = if multi_root {
                        format!("{base_label} · {}", root_display_name(&path.root))
                    } else {
                        base_label
                    };
                    let version = payload.version;
                    state.open_files.update(|files| {
                        files.insert(
                            path.clone(),
                            OpenFile {
                                path: payload.path,
                                version,
                                contents: payload.contents,
                                is_binary: payload.is_binary,
                            },
                        );
                    });
                    // Record the rendered version so code-intel frames apply
                    // only against this text (version-equals-rendered rule).
                    // §M4: when this is a *reload* to a newer version of an
                    // already-open file (external edit / branch switch / agent
                    // write), `set_rendered_version` drops the now-stale
                    // older-version decorations, and we send a fresh
                    // `code_intel_subscribe_file` so the server re-pushes the
                    // semantic model + diagnostics at the new version. The
                    // subscribe carries no version; the server peeks the same
                    // centralized counter that stamped these contents, so the
                    // re-push lands at exactly this rendered version.
                    if let Some(project_id) = resolve_project_id(&envelope.stream) {
                        let key = CodeIntelKey {
                            host_id: host_id.to_owned(),
                            project_id: project_id.clone(),
                            path: path.clone(),
                        };
                        let mut version_advanced = false;
                        state.code_intel.update(|map| {
                            let entry = map.entry(key).or_default();
                            let prior = entry.rendered_version;
                            entry.set_rendered_version(version);
                            version_advanced = matches!(prior, Some(prior) if version > prior);
                        });
                        if version_advanced {
                            refresh_code_intel_subscription(host_id, &project_id, &path);
                        }
                    }
                    // A background refresh (server reported this already-open
                    // file's version advanced, see `ProjectEventPayload::
                    // FilesChanged`) updates contents + code-intel in place;
                    // opening a tab here would steal focus (or hijack the active
                    // pane in single-pane mode). A normal user open has no
                    // pending-refresh marker and falls through to `open_tab`.
                    let was_refresh = state
                        .pending_file_refreshes
                        .with_untracked(|pending| pending.contains(&path));
                    if was_refresh {
                        state.pending_file_refreshes.update(|pending| {
                            pending.remove(&path);
                        });
                    } else {
                        state.open_tab(TabContent::File { path }, label, true);
                    }
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_file_contents payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectSearchResults => {
            match envelope.parse_payload::<ProjectSearchResultsPayload>() {
                Ok(payload) => {
                    state.search_state.update(|s| {
                        // Ignore results from a superseded / cancelled search.
                        if payload.search_id != s.active_search_id {
                            return;
                        }
                        s.total_matches += payload.file.matches.len() as u32;
                        s.total_files += 1;
                        s.results.push(payload.file);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_search_results payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectSearchComplete => {
            match envelope.parse_payload::<ProjectSearchCompletePayload>() {
                Ok(payload) => {
                    state.search_state.update(|s| {
                        if payload.search_id != s.active_search_id {
                            return;
                        }
                        s.in_flight = false;
                        // Trust the server's authoritative totals.
                        s.total_files = payload.total_files;
                        s.total_matches = payload.total_matches;
                        s.truncated = payload.truncated;
                        s.error = payload.error;
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_search_complete payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelStatus => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_status on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelStatusPayload>() {
                Ok(payload) => apply_code_intel_status(state, host_id, project_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_status payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelFileModel => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_file_model on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelFileModelPayload>() {
                Ok(payload) => {
                    let key = CodeIntelKey {
                        host_id: host_id.to_owned(),
                        project_id,
                        path: payload.path.clone(),
                    };
                    let version = payload.version;
                    // Only apply for a file that is currently open/subscribed.
                    // A frame that lands after the tab was closed must NOT
                    // resurrect the entry via `or_default()` (leak + stale).
                    if code_intel_file_is_open(state, &key) {
                        state.code_intel.update(|map| {
                            map.entry(key)
                                .or_default()
                                .merge_versioned(version, |data| data.merge_model(payload));
                        });
                    } else {
                        log::debug!(
                            "dropping code_intel_file_model for closed file {}",
                            payload.path.relative_path
                        );
                    }
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_file_model payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelDiagnostics => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_diagnostics on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelDiagnosticsPayload>() {
                Ok(payload) => {
                    let key = CodeIntelKey {
                        host_id: host_id.to_owned(),
                        project_id,
                        path: payload.path.clone(),
                    };
                    let version = payload.version;
                    // Like the file model: only apply for an open/subscribed
                    // file, and merge under the version-equals-rendered rule. A
                    // diagnostics frame is a full-file replace, so it overwrites
                    // the prior set wholesale rather than merging.
                    if code_intel_file_is_open(state, &key) {
                        state.code_intel.update(|map| {
                            map.entry(key)
                                .or_default()
                                .merge_versioned(version, |data| {
                                    data.diagnostics = payload.diagnostics;
                                });
                        });
                    } else {
                        log::debug!(
                            "dropping code_intel_diagnostics for closed file {}",
                            payload.path.relative_path
                        );
                    }
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_diagnostics payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelNavigateResult => {
            if resolve_project_id(&envelope.stream).is_none() {
                log::warn!(
                    "code_intel_navigate_result on non-project stream {}",
                    envelope.stream
                );
                return;
            }
            match envelope.parse_payload::<CodeIntelNavigateResultPayload>() {
                Ok(payload) => apply_code_intel_navigate_result(state, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_navigate_result payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelHoverResult => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_hover_result on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelHoverResultPayload>() {
                Ok(payload) => apply_code_intel_hover_result(state, host_id, project_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_hover_result payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelError => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!("code_intel_error on non-project stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<CodeIntelErrorPayload>() {
                Ok(payload) => apply_code_intel_error(state, host_id, project_id, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_error payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelReferencesResults => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_references_results on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelReferencesResultsPayload>() {
                Ok(payload) => {
                    apply_code_intel_references_results(state, host_id, project_id, payload)
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_references_results payload: {error}"),
                ),
            }
        }
        FrameKind::CodeIntelReferencesComplete => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "code_intel_references_complete on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<CodeIntelReferencesCompletePayload>() {
                Ok(payload) => {
                    apply_code_intel_references_complete(state, host_id, project_id, payload)
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse code_intel_references_complete payload: {error}"),
                ),
            }
        }
        FrameKind::NewTerminal => match envelope.parse_payload::<NewTerminalPayload>() {
            Ok(payload) => {
                let info = TerminalInfo {
                    host_id: host_id.to_string(),
                    terminal_id: payload.terminal_id,
                    stream: payload.stream,
                    project_id: None,
                    root: None,
                    cwd: String::new(),
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                    created_at_ms: 0,
                    pending_output: Vec::new(),
                    widget_mounted: false,
                    exited: false,
                    exit_code: None,
                    exit_signal: None,
                };
                state
                    .terminals
                    .update(|terminals| terminals.push(info.clone()));
                let force_focus = state
                    .pending_terminal_focus
                    .with_untracked(|p| p.as_deref() == Some(host_id));
                if force_focus || state.active_terminal.get_untracked().is_none() {
                    state.active_terminal.set(Some(ActiveTerminalRef {
                        host_id: info.host_id,
                        terminal_id: info.terminal_id,
                    }));
                }
                if force_focus {
                    state.pending_terminal_focus.set(None);
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse new_terminal payload: {error}"),
            ),
        },
        FrameKind::TerminalStart => match envelope.parse_payload::<TerminalStartPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch terminal_start host={} stream={} project_id={:?} cwd={} shell={}",
                    host_id,
                    envelope.stream,
                    payload.project_id,
                    payload.cwd,
                    payload.shell
                );
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        terminal.project_id = payload.project_id;
                        terminal.root = payload.root;
                        terminal.cwd = payload.cwd;
                        terminal.shell = payload.shell;
                        terminal.cols = payload.cols;
                        terminal.rows = payload.rows;
                        terminal.created_at_ms = payload.created_at_ms;
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_start payload: {error}"),
            ),
        },
        FrameKind::TerminalOutput => match envelope.parse_payload::<TerminalOutputPayload>() {
            Ok(payload) => {
                // Fast path: terminal widget is already mounted, so we
                // just write the bytes straight to xterm — no reactive
                // state needs to change. The original code called
                // `state.terminals.update(...)` regardless, which fires
                // on every output chunk (often hundreds per second from
                // a noisy build) and notifies every subscriber of the
                // terminals list, e.g. the dock-zone tab counter and
                // any panel that lists terminals. Each notify ran a
                // full re-render of those subtrees while xterm itself
                // did the only meaningful work.
                let mounted_tid = state.terminals.with_untracked(|terminals| {
                    terminals
                        .iter()
                        .find(|terminal| {
                            terminal.host_id == host_id
                                && terminal.stream == envelope.stream
                                && terminal.widget_mounted
                        })
                        .map(|terminal| terminal.terminal_id.0.clone())
                });
                if let Some(tid) = mounted_tid {
                    crate::term_bridge::write(&tid, &payload.data);
                } else {
                    // Slow path: widget hasn't mounted yet (or the
                    // terminal isn't tracked). Buffer pending output so
                    // the widget can flush it at mount time. This
                    // *does* mutate state and so must go through
                    // `update`.
                    state.terminals.update(|terminals| {
                        if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                            terminal.host_id == host_id && terminal.stream == envelope.stream
                        }) {
                            terminal.pending_output.push(payload.data.clone());
                        }
                    });
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_output payload: {error}"),
            ),
        },
        FrameKind::TerminalExit => match envelope.parse_payload::<TerminalExitPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch terminal_exit host={} stream={} exit_code={:?} signal={:?}",
                    host_id,
                    envelope.stream,
                    payload.exit_code,
                    payload.signal
                );
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        terminal.exited = true;
                        terminal.exit_code = payload.exit_code;
                        terminal.exit_signal = payload.signal;
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_exit payload: {error}"),
            ),
        },
        FrameKind::TerminalError => match envelope.parse_payload::<TerminalErrorPayload>() {
            Ok(payload) => {
                log::error!("terminal error ({:?}): {}", payload.code, payload.message);
                if payload.fatal {
                    state.terminals.update(|terminals| {
                        if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                            terminal.host_id == host_id && terminal.stream == envelope.stream
                        }) {
                            terminal.exited = true;
                        }
                    });
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_error payload: {error}"),
            ),
        },
        FrameKind::HostBrowseOpened => match envelope.parse_payload::<HostBrowseOpenedPayload>() {
            Ok(payload) => dispatch_browse_opened(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_browse_opened payload: {error}"),
            ),
        },
        FrameKind::HostBrowseEntries => {
            match envelope.parse_payload::<HostBrowseEntriesPayload>() {
                Ok(payload) => dispatch_browse_entries(state, host_id, &envelope.stream, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse host_browse_entries payload: {error}"),
                ),
            }
        }
        FrameKind::HostBrowseError => match envelope.parse_payload::<HostBrowseErrorPayload>() {
            Ok(payload) => dispatch_browse_error(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_browse_error payload: {error}"),
            ),
        },
        FrameKind::LaunchProfileCatalogNotify => {
            match envelope.parse_payload::<LaunchProfileCatalogPayload>() {
                Ok(payload) => {
                    state.launch_profile_catalog.update(|map| {
                        map.insert(host_id.to_string(), payload.catalog);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse launch_profile_catalog payload: {error}"),
                ),
            }
        }
        FrameKind::CustomAgentNotify => {
            match envelope.parse_payload::<CustomAgentNotifyPayload>() {
                Ok(CustomAgentNotifyPayload::Upsert { custom_agent }) => {
                    state.custom_agents.update(|map| {
                        let host_map = map.entry(host_id.to_string()).or_default();
                        host_map.insert(custom_agent.id.clone(), custom_agent);
                    });
                }
                Ok(CustomAgentNotifyPayload::Delete { id }) => {
                    state.custom_agents.update(|map| {
                        if let Some(host_map) = map.get_mut(host_id) {
                            host_map.remove(&id);
                        }
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse custom_agent_notify payload: {error}"),
                ),
            }
        }
        FrameKind::McpServerNotify => match envelope.parse_payload::<McpServerNotifyPayload>() {
            Ok(McpServerNotifyPayload::Upsert { mcp_server }) => {
                state.mcp_servers.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(mcp_server.id.clone(), mcp_server);
                });
            }
            Ok(McpServerNotifyPayload::Delete { id }) => {
                state.mcp_servers.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse mcp_server_notify payload: {error}"),
            ),
        },
        FrameKind::WorkflowNotify => match envelope.parse_payload::<WorkflowNotifyPayload>() {
            Ok(payload) => {
                state.workflow_summaries.update(|map| {
                    map.insert(host_id.to_string(), payload.summaries);
                });
                state.workflow_diagnostics.update(|map| {
                    map.insert(host_id.to_string(), payload.diagnostics);
                });
                state.workflow_locations.update(|map| {
                    map.insert(host_id.to_string(), payload.locations);
                });
                // A successful catalog notify clears a prior refresh failure.
                clear_workflow_error_for_kinds(state, host_id, &[FrameKind::WorkflowRefresh]);
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse workflow_notify payload: {error}"),
            ),
        },
        FrameKind::WorkflowRunNotify => {
            match envelope.parse_payload::<WorkflowRunNotifyPayload>() {
                Ok(payload) => {
                    state.workflow_runs.update(|map| {
                        let host_map = map.entry(host_id.to_string()).or_default();
                        host_map.insert(payload.run.id.clone(), payload.run);
                    });
                    // A successful run notify clears a prior trigger/cancel
                    // failure for this host.
                    clear_workflow_error_for_kinds(
                        state,
                        host_id,
                        &[FrameKind::TriggerWorkflow, FrameKind::CancelWorkflow],
                    );
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse workflow_run_notify payload: {error}"),
                ),
            }
        }
        FrameKind::SteeringNotify => match envelope.parse_payload::<SteeringNotifyPayload>() {
            Ok(SteeringNotifyPayload::Upsert { steering }) => {
                state.steering.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(steering.id.clone(), steering);
                });
            }
            Ok(SteeringNotifyPayload::Delete { id }) => {
                state.steering.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse steering_notify payload: {error}"),
            ),
        },
        FrameKind::SkillNotify => match envelope.parse_payload::<SkillNotifyPayload>() {
            Ok(SkillNotifyPayload::Upsert { skill }) => {
                state.skills.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(skill.id.clone(), skill);
                });
            }
            Ok(SkillNotifyPayload::Delete { id }) => {
                state.skills.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse skill_notify payload: {error}"),
            ),
        },
        FrameKind::ReviewEvent => {
            let Some(review_id) = resolve_review_id(&envelope.stream) else {
                log::warn!("review_event on non-review stream {}", envelope.stream);
                return;
            };
            let parse_t0 = crate::perf::now_ms();
            let payload_bytes = envelope.payload.to_string().len();
            match envelope.parse_payload::<ReviewEventPayload>() {
                Ok(payload) => {
                    let parse_dt = crate::perf::now_ms() - parse_t0;
                    let variant = match &payload {
                        ReviewEventPayload::Snapshot { .. } => "Snapshot",
                        ReviewEventPayload::CommentUpsert { .. } => "CommentUpsert",
                        ReviewEventPayload::CommentDelete { .. } => "CommentDelete",
                        ReviewEventPayload::SuggestionUpsert { .. } => "SuggestionUpsert",
                        ReviewEventPayload::AiReviewerChanged { .. } => "AiReviewerChanged",
                        ReviewEventPayload::StatusChanged { .. } => "StatusChanged",
                        ReviewEventPayload::Cleared { .. } => "Cleared",
                        ReviewEventPayload::Error { .. } => "Error",
                    };
                    let key = format!("review:{}", review_id.0);
                    crate::perf::log_phase(
                        "review_dispatch",
                        "parsed",
                        &key,
                        &format!(" variant={variant} bytes={payload_bytes} took={parse_dt:.1}ms"),
                    );
                    let apply_t0 = crate::perf::now_ms();
                    apply_review_event(state, &review_id, payload);
                    let apply_dt = crate::perf::now_ms() - apply_t0;
                    crate::perf::log_phase(
                        "review_dispatch",
                        "applied",
                        &key,
                        &format!(" variant={variant} took={apply_dt:.1}ms"),
                    );
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse review_event payload: {error}"),
                ),
            }
        }
        FrameKind::TeamNotify => match envelope.parse_payload::<TeamNotifyPayload>() {
            Ok(TeamNotifyPayload::Upsert { team }) => {
                state.teams.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(team.id.clone(), team);
                });
            }
            Ok(TeamNotifyPayload::Delete { team }) => {
                state.teams.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&team.id);
                    }
                });
                // Drop any members and their bindings that belonged to this
                // team. Snapshot dropped ids before the retain so the binding
                // prune knows which to remove.
                let dropped_member_ids = state.team_members.with_untracked(|map| {
                    map.get(host_id)
                        .map(|m| {
                            m.iter()
                                .filter(|(_, member)| member.team_id == team.id)
                                .map(|(id, _)| id.clone())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
                if !dropped_member_ids.is_empty() {
                    state.team_members.update(|map| {
                        if let Some(host_map) = map.get_mut(host_id) {
                            host_map.retain(|_, member| member.team_id != team.id);
                        }
                    });
                    state.team_member_bindings.update(|map| {
                        if let Some(host_map) = map.get_mut(host_id) {
                            host_map.retain(|member_id, _| !dropped_member_ids.contains(member_id));
                        }
                    });
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse team_notify payload: {error}"),
            ),
        },
        FrameKind::TeamMemberNotify => match envelope.parse_payload::<TeamMemberNotifyPayload>() {
            Ok(TeamMemberNotifyPayload::Upsert { member }) => {
                state.team_members.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(member.id.clone(), member);
                });
            }
            Ok(TeamMemberNotifyPayload::Delete { member }) => {
                state.team_members.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&member.id);
                    }
                });
                state.team_member_bindings.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&member.id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse team_member_notify payload: {error}"),
            ),
        },
        FrameKind::TeamMemberBindingNotify => {
            match envelope.parse_payload::<TeamMemberBindingNotifyPayload>() {
                Ok(TeamMemberBindingNotifyPayload::Upsert { binding }) => {
                    state.team_member_bindings.update(|map| {
                        let host_map = map.entry(host_id.to_string()).or_default();
                        host_map.insert(binding.member_id.clone(), binding);
                    });
                }
                Ok(TeamMemberBindingNotifyPayload::Delete { binding }) => {
                    state.team_member_bindings.update(|map| {
                        if let Some(host_map) = map.get_mut(host_id) {
                            host_map.remove(&binding.member_id);
                        }
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse team_member_binding_notify payload: {error}"),
                ),
            }
        }
        FrameKind::TeamPresetCatalogNotify => {
            match envelope.parse_payload::<TeamPresetCatalogNotifyPayload>() {
                Ok(payload) => {
                    state.team_preset_catalogs.update(|map| {
                        map.insert(host_id.to_string(), payload.catalog);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse team_preset_catalog_notify payload: {error}"),
                ),
            }
        }
        FrameKind::TeamDraftNotify => match envelope.parse_payload::<TeamDraftNotifyPayload>() {
            Ok(TeamDraftNotifyPayload::Upsert { draft }) => {
                state.team_drafts.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(draft.id.clone(), draft);
                });
            }
            Ok(TeamDraftNotifyPayload::Delete { draft_id }) => {
                state.team_drafts.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&draft_id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse team_draft_notify payload: {error}"),
            ),
        },
        FrameKind::TeamMemberShuffleSuggestionNotify => {
            match envelope.parse_payload::<TeamMemberShuffleSuggestionNotifyPayload>() {
                Ok(payload) => {
                    state.record_team_member_shuffle_suggestion(host_id, payload);
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!(
                        "failed to parse team_member_shuffle_suggestion_notify payload: {error}"
                    ),
                ),
            }
        }
        _ => {
            log::warn!("unexpected frame kind from server: {}", envelope.kind);
        }
    }
}

/// Apply a `code_intel_status` frame to the per-file signal. Only file-scoped
/// statuses carry a version and a path; they merge under the
/// version-equals-rendered rule. Project/Provider-scoped statuses are not yet
/// surfaced per-file in M0 (no key to attach them to) and are logged.
fn apply_code_intel_status(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: CodeIntelStatusPayload,
) {
    let CodeIntelStatusScope::File { path, version } = payload.scope.clone() else {
        log::debug!("code_intel_status (non-file scope) ignored in M0 frontend");
        return;
    };
    let key = CodeIntelKey {
        host_id: host_id.to_owned(),
        project_id,
        path: path.clone(),
    };
    // Only apply for a file that is currently open/subscribed; a status frame
    // that arrives after the tab closed must not resurrect a cleaned-up entry.
    if !code_intel_file_is_open(state, &key) {
        log::debug!(
            "dropping code_intel_status for closed file {}",
            path.relative_path
        );
        return;
    }
    state.code_intel.update(|map| {
        map.entry(key)
            .or_default()
            .merge_versioned(version, |data| data.status = Some(payload));
    });
}

/// Send a fresh `code_intel_subscribe_file` after an open file reloaded at a
/// newer version (§M4), so the server re-pushes the semantic model + diagnostics
/// for the new rendered version. The network send is wasm-only (there is no
/// transport off the browser); on the native test build this is a no-op and the
/// signal-level reset (`set_rendered_version` dropping stale data) is the whole
/// observable effect.
#[cfg(target_arch = "wasm32")]
fn refresh_code_intel_subscription(host_id: &str, project_id: &ProjectId, path: &ProjectPath) {
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = protocol::CodeIntelSubscribeFilePayload { path: path.clone() };
    let host_id = host_id.to_owned();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = crate::send::send_frame(
            &host_id,
            stream,
            protocol::FrameKind::CodeIntelSubscribeFile,
            &payload,
        )
        .await
        {
            log::error!("failed to send code-intel refresh subscribe: {error}");
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn refresh_code_intel_subscription(_host_id: &str, _project_id: &ProjectId, _path: &ProjectPath) {}

/// Whether a code-intel frame for `key` should be applied: true only if the
/// file is currently open/subscribed — either it already has a code-intel entry
/// or its contents are in `open_files`. Used to drop late frames that arrive
/// after a tab closed (and was unsubscribed) instead of resurrecting state.
fn code_intel_file_is_open(state: &AppState, key: &CodeIntelKey) -> bool {
    state.code_intel.with_untracked(|map| map.contains_key(key))
        || state
            .open_files
            .with_untracked(|files| files.contains_key(&key.path))
}

fn record_code_intel_error_for_path(
    state: &AppState,
    host_id: &str,
    project_id: &ProjectId,
    path: ProjectPath,
    payload: CodeIntelErrorPayload,
) {
    let key = CodeIntelKey {
        host_id: host_id.to_owned(),
        project_id: project_id.clone(),
        path: path.clone(),
    };
    if !code_intel_file_is_open(state, &key) {
        log::debug!(
            "dropping code_intel_error for closed file {}",
            path.relative_path
        );
        return;
    }
    let version = state
        .code_intel
        .with_untracked(|map| map.get(&key).and_then(|file| file.rendered_version))
        .or_else(|| {
            state
                .open_files
                .with_untracked(|files| files.get(&path).map(|file| file.version))
        });
    let Some(version) = version else {
        return;
    };
    state.code_intel.update(|map| {
        map.entry(key)
            .or_default()
            .merge_versioned(version, |data| data.error = Some(payload));
    });
}

fn references_context_matches(
    state: &ProjectReferencesUiState,
    active_project: Option<&crate::state::ActiveProjectRef>,
    host_id: &str,
    project_id: &ProjectId,
    references_id: u64,
) -> bool {
    state.active_references_id == references_id
        && state.host_id.as_deref() == Some(host_id)
        && state.project_id.as_ref() == Some(project_id)
        && active_project
            .map(|active| active.host_id == host_id && &active.project_id == project_id)
            .unwrap_or(false)
}

fn definition_target_line(state: &AppState, target: &CodeIntelLocation) -> CodeIntelReferenceLine {
    let contents = state.open_files.with_untracked(|files| {
        files
            .get(&target.path)
            .and_then(|file| file.contents.as_ref().cloned())
    });
    let Some(contents) = contents else {
        return CodeIntelReferenceLine {
            line_number: 0,
            line_text: format!(
                "{}: byte {} (preview unavailable)",
                target.path.relative_path, target.range.start
            ),
            ranges: Vec::new(),
        };
    };
    let lines = FileLines::new(&contents);
    if lines.len() == 0 {
        return CodeIntelReferenceLine {
            line_number: 0,
            line_text: "(empty file)".to_owned(),
            ranges: Vec::new(),
        };
    }
    let line_index = lines.line_for_byte(target.range.start);
    let line_start = lines.line_start(line_index);
    let line_end = lines.line_content_end(line_index);
    let start = target
        .range
        .start
        .clamp(line_start, line_end)
        .saturating_sub(line_start);
    let end = target
        .range
        .end
        .clamp(line_start, line_end)
        .saturating_sub(line_start)
        .max(start);
    let ranges = if start < end {
        vec![ByteRange { start, end }]
    } else {
        Vec::new()
    };
    CodeIntelReferenceLine {
        line_number: (line_index + 1) as u32,
        line_text: lines.line(line_index).to_owned(),
        ranges,
    }
}

fn definition_target_results(
    state: &AppState,
    targets: &[CodeIntelLocation],
) -> (Vec<CodeIntelReferencesFileResult>, Vec<CodeIntelLocation>) {
    let mut grouped: Vec<(CodeIntelReferencesFileResult, Vec<CodeIntelLocation>)> = Vec::new();
    for target in targets {
        let line = definition_target_line(state, target);
        if let Some((file, row_targets)) = grouped
            .iter_mut()
            .find(|(file, _)| file.path == target.path)
        {
            file.lines.push(line);
            row_targets.push(target.clone());
        } else {
            grouped.push((
                CodeIntelReferencesFileResult {
                    path: target.path.clone(),
                    lines: vec![line],
                    truncated: false,
                },
                vec![target.clone()],
            ));
        }
    }
    let mut results = Vec::with_capacity(grouped.len());
    let mut row_targets = Vec::with_capacity(targets.len());
    for (file, targets) in grouped {
        row_targets.extend(targets);
        results.push(file);
    }
    (results, row_targets)
}

fn apply_code_intel_error(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: CodeIntelErrorPayload,
) {
    match payload.context.clone() {
        CodeIntelErrorContext::Subscribe { path }
        | CodeIntelErrorContext::Hover { path, .. }
        | CodeIntelErrorContext::Navigate { path, .. } => {
            record_code_intel_error_for_path(state, host_id, &project_id, path, payload);
        }
        CodeIntelErrorContext::FindReferences {
            references_id,
            path,
        } => {
            record_code_intel_error_for_path(state, host_id, &project_id, path, payload.clone());
            let active = state.active_project_ref_untracked();
            state.references_state.update(|s| {
                if !references_context_matches(
                    s,
                    active.as_ref(),
                    host_id,
                    &project_id,
                    references_id,
                ) {
                    return;
                }
                s.in_flight = false;
                s.cancelled = false;
                s.error = Some(payload.message.clone());
            });
        }
        CodeIntelErrorContext::Provider { .. } => {
            let keys = state.code_intel.with_untracked(|map| {
                map.keys()
                    .filter(|key| key.host_id == host_id && key.project_id == project_id)
                    .cloned()
                    .collect::<Vec<_>>()
            });
            state.code_intel.update(|map| {
                for key in keys {
                    if let Some(version) = map.get(&key).and_then(|file| file.rendered_version) {
                        map.entry(key.clone())
                            .or_default()
                            .merge_versioned(version, |data| data.error = Some(payload.clone()));
                    }
                }
            });
        }
    }
}

/// Apply a `code_intel_navigate_result` (M2 go-to-definition miss-fill).
/// Ignored if superseded (a newer `navigate_id` was sent). Empty `targets` is an
/// honest "no definition here" — nothing happens. A single target opens
/// directly; multiple targets populate the reusable references panel as a
/// chooser and the user's row click performs the byte-offset jump.
pub(crate) fn apply_code_intel_navigate_result(
    state: &AppState,
    payload: CodeIntelNavigateResultPayload,
) {
    // The result must still match the full context we recorded when sending:
    // same navigate id, same owning host/project (no project switch), and the
    // source file still open at the same rendered version. Otherwise the user
    // has moved on and navigating would be a surprise jump — drop it.
    let Some(ctx) = state.code_intel_navigate_ctx.get_untracked() else {
        return;
    };
    if payload.navigate_id != ctx.navigate_id {
        return; // superseded by a newer navigate request
    }
    let Some(active) = state.active_project_ref_untracked() else {
        return;
    };
    if active.host_id != ctx.host_id || active.project_id != ctx.project_id {
        return; // project switched since the request
    }
    let source_key = CodeIntelKey {
        host_id: ctx.host_id.clone(),
        project_id: ctx.project_id.clone(),
        path: payload.path.clone(),
    };
    let source_current = state.open_files.with_untracked(|files| {
        files
            .get(&payload.path)
            .map(|file| file.version == payload.version)
            .unwrap_or(false)
    }) && state.code_intel.with_untracked(|map| {
        map.get(&source_key)
            .map(|file| file.rendered_version == Some(payload.version))
            .unwrap_or(false)
    });
    if !source_current {
        return; // source file closed or changed version since the request
    }
    // Consume the context so a duplicate/late result can't re-fire.
    state.code_intel_navigate_ctx.set(None);

    let mut targets = payload.targets;
    if targets.is_empty() {
        log::debug!("code_intel_navigate_result: no definition at this position");
        return;
    }
    if targets.len() == 1 {
        let target = targets.remove(0);
        state
            .pending_goto_offset
            .set(Some((target.path.clone(), target.range.start)));
        crate::actions::open_project_path(state, target.path);
        return;
    }

    let (results, row_targets) = definition_target_results(state, &targets);
    let total_files = results.len() as u32;
    let total_references = row_targets.len() as u32;
    state.references_state.set(ProjectReferencesUiState {
        mode: ProjectReferencesMode::DefinitionTargets,
        host_id: Some(ctx.host_id),
        project_id: Some(ctx.project_id),
        source_path: Some(ctx.path),
        source_version: Some(ctx.version),
        active_references_id: 0,
        in_flight: false,
        symbol: None,
        results,
        row_targets,
        total_files,
        total_references,
        truncated: false,
        cancelled: false,
        error: None,
    });
    state.left_tab.set(crate::state::LeftTab::References);
}

/// Apply a `code_intel_hover_result`. Ignored if superseded. Honors the
/// version-equals-rendered rule (a hover computed against text the user no
/// longer views is dropped). Empty/`None` contents dismisses the popover
/// ("nothing to show here"); otherwise the markdown fills the popover that was
/// seeded with the anchor rect at request time.
fn apply_code_intel_hover_result(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: CodeIntelHoverResultPayload,
) {
    if payload.hover_id != state.code_intel_active_hover.get_untracked() {
        return; // superseded
    }
    let key = CodeIntelKey {
        host_id: host_id.to_owned(),
        project_id,
        path: payload.path.clone(),
    };
    let version_matches = state.code_intel.with_untracked(|map| {
        map.get(&key)
            .map(|file| file.rendered_version == Some(payload.version))
            .unwrap_or(false)
    });
    let contents = match (version_matches, payload.contents) {
        (true, Some(text)) if !text.trim().is_empty() => Some(text),
        _ => None,
    };
    state.code_intel_hover.update(|hover| {
        // Only fill the popover we seeded for this exact hover id.
        let matches = hover
            .as_ref()
            .map(|popover| popover.hover_id == payload.hover_id)
            .unwrap_or(false);
        if !matches {
            return;
        }
        match contents {
            Some(text) => {
                if let Some(popover) = hover.as_mut() {
                    popover.contents = Some(text);
                }
            }
            None => *hover = None, // nothing to show: dismiss
        }
    });
}

/// Apply a streamed `code_intel_references_results` frame (M5): append one
/// matching file's references to the panel. Frames carrying a stale/superseded
/// `references_id` (a newer query is active, or the panel was cleared) are
/// dropped — the active id is the correlation, exactly like `search_id`.
fn apply_code_intel_references_results(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: CodeIntelReferencesResultsPayload,
) {
    let active = state.active_project_ref_untracked();
    state.references_state.update(|s| {
        if !references_context_matches(
            s,
            active.as_ref(),
            host_id,
            &project_id,
            payload.references_id,
        ) {
            return; // superseded / cleared / inactive project
        }
        s.results.push(payload.file);
        s.total_files = s.results.len() as u32;
        s.total_references = s
            .results
            .iter()
            .map(|file| {
                file.lines
                    .iter()
                    .map(|line| line.ranges.len() as u32)
                    .sum::<u32>()
            })
            .sum();
        s.truncated = s.results.iter().any(|file| file.truncated);
    });
}

/// Apply the terminal `code_intel_references_complete` frame (M5): record the
/// totals / truncation / cancellation / error and mark the query finished.
/// Dropped if it doesn't match the active `references_id`.
fn apply_code_intel_references_complete(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: CodeIntelReferencesCompletePayload,
) {
    let active = state.active_project_ref_untracked();
    state.references_state.update(|s| {
        if !references_context_matches(
            s,
            active.as_ref(),
            host_id,
            &project_id,
            payload.references_id,
        ) {
            return; // superseded / cleared / inactive project
        }
        s.in_flight = false;
        s.total_files = payload.total_files;
        s.total_references = payload.total_references;
        s.truncated = payload.truncated;
        s.cancelled = payload.cancelled;
        s.error = payload.error;
    });
}

fn resolve_review_id(stream: &StreamPath) -> Option<ReviewId> {
    let suffix = stream.0.strip_prefix("/review/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ReviewId(suffix.to_string()))
}

fn apply_review_event(state: &AppState, review_id: &ReviewId, payload: ReviewEventPayload) {
    match payload {
        ReviewEventPayload::Snapshot { review } => {
            let comments = review.comments.len();
            let suggestions = review.suggestions.len();
            let diffs = review.diffs.len();
            let action_gate_before = state
                .review_action_pending
                .with_untracked(|m| m.get(review_id).copied().unwrap_or_default());
            let target_gate_before = state
                .review_action_target_pending
                .with_untracked(|set| set.iter().filter(|(rid, _)| rid == review_id).count());
            state.reviews.update(|map| {
                map.insert(review_id.clone(), review);
            });
            // A snapshot from a fresh subscription means any in-flight
            // submit/cancel/etc. echoes have already been folded in.
            state.review_action_pending.update(|map| {
                map.remove(review_id);
            });
            state.review_action_target_pending.update(|set| {
                set.retain(|(rid, _)| rid != review_id);
            });
            log::info!(
                "review.event.snapshot review={review_id} comments={comments} suggestions={suggestions} diffs={diffs} cleared_action_gate={:?} cleared_target_gates={target_gate_before}",
                action_gate_before
            );
        }
        ReviewEventPayload::CommentUpsert { comment } => {
            let was_new = state.reviews.with_untracked(|map| {
                map.get(review_id)
                    .map(|r| !r.comments.iter().any(|c| c.id == comment.id))
                    .unwrap_or(false)
            });
            state.reviews.update(|map| {
                let Some(review) = map.get_mut(review_id) else {
                    log::warn!(
                        "review CommentUpsert for unknown review {review_id} — \
                         dropped (Snapshot will resync on resubscribe)"
                    );
                    return;
                };
                if let Some(existing) = review.comments.iter_mut().find(|c| c.id == comment.id) {
                    *existing = comment.clone();
                } else {
                    review.comments.push(comment.clone());
                }
            });
            let mut cleared_add = false;
            let mut cleared_update = false;
            let mut cleared_accept = false;
            state.review_action_target_pending.update(|set| {
                // New User comment — clear the AddComment gate (composer
                // closes from its own effect on seeing the new comment).
                if was_new && matches!(comment.source, ReviewCommentSource::User) {
                    cleared_add = set.remove(&(review_id.clone(), ReviewActionTarget::AddComment));
                }
                // Existing comment updated — clear UpdateComment gate.
                if !was_new {
                    cleared_update = set.remove(&(
                        review_id.clone(),
                        ReviewActionTarget::UpdateComment(comment.id.clone()),
                    ));
                }
                // Newly-created AI-suggestion-derived comment ⇒ matching
                // AcceptSuggestion gate clears.
                if let ReviewCommentSource::AiSuggestion { suggestion_id, .. } = &comment.source {
                    cleared_accept = set.remove(&(
                        review_id.clone(),
                        ReviewActionTarget::AcceptSuggestion(suggestion_id.clone()),
                    ));
                }
            });
            log::info!(
                "review.event.comment_upsert review={review_id} comment_id={} was_new={was_new} cleared_add={cleared_add} cleared_update={cleared_update} cleared_accept={cleared_accept}",
                comment.id
            );
        }
        ReviewEventPayload::CommentDelete { comment_id } => {
            state.reviews.update(|map| {
                let Some(review) = map.get_mut(review_id) else {
                    return;
                };
                review.comments.retain(|c| c.id != comment_id);
            });
            let mut cleared = false;
            state.review_action_target_pending.update(|set| {
                cleared = set.remove(&(
                    review_id.clone(),
                    ReviewActionTarget::DeleteComment(comment_id.clone()),
                ));
            });
            log::info!(
                "review.event.comment_delete review={review_id} comment_id={comment_id} cleared_delete={cleared}"
            );
        }
        ReviewEventPayload::SuggestionUpsert { suggestion } => {
            state.reviews.update(|map| {
                let Some(review) = map.get_mut(review_id) else {
                    return;
                };
                if let Some(existing) = review
                    .suggestions
                    .iter_mut()
                    .find(|s| s.id == suggestion.id)
                {
                    *existing = suggestion.clone();
                } else {
                    review.suggestions.push(suggestion.clone());
                }
            });
            let mut cleared_accept = false;
            let mut cleared_reject = false;
            let suggestion_state_label = match &suggestion.state {
                ReviewSuggestionState::Pending => "pending",
                ReviewSuggestionState::Accepted { .. } => "accepted",
                ReviewSuggestionState::Rejected => "rejected",
            };
            state
                .review_action_target_pending
                .update(|set| match &suggestion.state {
                    ReviewSuggestionState::Accepted { .. } => {
                        cleared_accept = set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::AcceptSuggestion(suggestion.id.clone()),
                        ));
                    }
                    ReviewSuggestionState::Rejected => {
                        cleared_reject = set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::RejectSuggestion(suggestion.id.clone()),
                        ));
                    }
                    ReviewSuggestionState::Pending => {}
                });
            log::info!(
                "review.event.suggestion_upsert review={review_id} suggestion_id={} state={suggestion_state_label} cleared_accept={cleared_accept} cleared_reject={cleared_reject}",
                suggestion.id
            );
        }
        ReviewEventPayload::AiReviewerChanged { state: ai_state } => {
            let ai_status_label = match ai_state.status {
                protocol::ReviewAiReviewerStatus::Idle => "idle",
                protocol::ReviewAiReviewerStatus::Running => "running",
                protocol::ReviewAiReviewerStatus::Completed => "completed",
                protocol::ReviewAiReviewerStatus::Failed => "failed",
            };
            let gate_before = state
                .review_action_pending
                .with_untracked(|m| m.get(review_id).copied().unwrap_or_default());
            state.reviews.update(|map| {
                if let Some(review) = map.get_mut(review_id) {
                    review.ai_reviewer = ai_state;
                }
            });
            state.review_action_pending.update(|map| {
                if let Some(gate) = map.get_mut(review_id) {
                    gate.start_ai = false;
                    if gate.is_idle() {
                        map.remove(review_id);
                    }
                }
            });
            log::info!(
                "review.event.ai_reviewer_changed review={review_id} ai_status={ai_status_label} cleared_start_ai={}",
                gate_before.start_ai
            );
        }
        ReviewEventPayload::StatusChanged { status } => {
            let status_label = match &status {
                protocol::ReviewStatus::Draft => "draft",
                protocol::ReviewStatus::Submitted { .. } => "submitted",
                protocol::ReviewStatus::Consumed { .. } => "consumed",
                protocol::ReviewStatus::Cancelled { .. } => "cancelled",
            };
            let gate_before = state
                .review_action_pending
                .with_untracked(|m| m.get(review_id).copied().unwrap_or_default());
            state.reviews.update(|map| {
                if let Some(review) = map.get_mut(review_id) {
                    review.status = status;
                }
            });
            state.review_action_pending.update(|map| {
                if let Some(gate) = map.get_mut(review_id) {
                    gate.submit = false;
                    gate.cancel = false;
                    if gate.is_idle() {
                        map.remove(review_id);
                    }
                }
            });
            log::info!(
                "review.event.status_changed review={review_id} status={status_label} cleared_submit={} cleared_cancel={}",
                gate_before.submit,
                gate_before.cancel
            );
        }
        ReviewEventPayload::Cleared { review } => {
            // A reset of the project-scoped review: emitted after a
            // successful Submit, an explicit ClearComments, or a clean
            // working tree. The included `review` is the fresh
            // (comment/suggestion-free) projection — replace our local
            // copy wholesale and drop every in-flight gate, since nothing
            // the user had queued still applies to the reset review.
            let comments = review.comments.len();
            let suggestions = review.suggestions.len();
            state.reviews.update(|map| {
                map.insert(review_id.clone(), review);
            });
            state.review_action_pending.update(|map| {
                map.remove(review_id);
            });
            state.review_action_target_pending.update(|set| {
                set.retain(|(rid, _)| rid != review_id);
            });
            log::info!(
                "review.event.cleared review={review_id} comments={comments} suggestions={suggestions}"
            );
        }
        ReviewEventPayload::Error { error } => {
            log::error!(
                "review {review_id} server error code={:?} fatal={} context={:?}: {}",
                error.code,
                error.fatal,
                error.context,
                error.message
            );
            let action_gate_before = state
                .review_action_pending
                .with_untracked(|m| m.get(review_id).copied().unwrap_or_default());
            let target_gate_count_before = state
                .review_action_target_pending
                .with_untracked(|set| set.iter().filter(|(rid, _)| rid == review_id).count());
            let context_label = match &error.context {
                ReviewErrorContext::AddComment => "add_comment",
                ReviewErrorContext::UpdateComment { .. } => "update_comment",
                ReviewErrorContext::DeleteComment { .. } => "delete_comment",
                ReviewErrorContext::AcceptSuggestion { .. } => "accept_suggestion",
                ReviewErrorContext::RejectSuggestion { .. } => "reject_suggestion",
                ReviewErrorContext::StartAiReview => "start_ai",
                ReviewErrorContext::Submit => "submit",
                ReviewErrorContext::ClearComments => "clear_comments",
                ReviewErrorContext::Cancel => "cancel",
            };
            log::info!(
                "review.event.error.gate_clear_before review={review_id} context={context_label} action_gate={:?} target_gates={target_gate_count_before}",
                action_gate_before
            );
            // Clear only the gate matching the error context.
            match error.context {
                ReviewErrorContext::AddComment => {
                    state.review_action_target_pending.update(|set| {
                        set.remove(&(review_id.clone(), ReviewActionTarget::AddComment));
                    });
                }
                ReviewErrorContext::UpdateComment { comment_id } => {
                    state.review_action_target_pending.update(|set| {
                        set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::UpdateComment(comment_id),
                        ));
                    });
                }
                ReviewErrorContext::DeleteComment { comment_id } => {
                    state.review_action_target_pending.update(|set| {
                        set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::DeleteComment(comment_id),
                        ));
                    });
                }
                ReviewErrorContext::AcceptSuggestion { suggestion_id } => {
                    state.review_action_target_pending.update(|set| {
                        set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::AcceptSuggestion(suggestion_id),
                        ));
                    });
                }
                ReviewErrorContext::RejectSuggestion { suggestion_id } => {
                    state.review_action_target_pending.update(|set| {
                        set.remove(&(
                            review_id.clone(),
                            ReviewActionTarget::RejectSuggestion(suggestion_id),
                        ));
                    });
                }
                ReviewErrorContext::StartAiReview => {
                    state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(review_id) {
                            gate.start_ai = false;
                            if gate.is_idle() {
                                map.remove(review_id);
                            }
                        }
                    });
                }
                ReviewErrorContext::Submit => {
                    state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(review_id) {
                            gate.submit = false;
                            if gate.is_idle() {
                                map.remove(review_id);
                            }
                        }
                    });
                }
                ReviewErrorContext::ClearComments => {
                    state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(review_id) {
                            gate.clear = false;
                            if gate.is_idle() {
                                map.remove(review_id);
                            }
                        }
                    });
                }
                ReviewErrorContext::Cancel => {
                    state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(review_id) {
                            gate.cancel = false;
                            if gate.is_idle() {
                                map.remove(review_id);
                            }
                        }
                    });
                }
            }
            let action_gate_after = state
                .review_action_pending
                .with_untracked(|m| m.get(review_id).copied().unwrap_or_default());
            let target_gate_count_after = state
                .review_action_target_pending
                .with_untracked(|set| set.iter().filter(|(rid, _)| rid == review_id).count());
            log::info!(
                "review.event.error.gate_clear_after review={review_id} context={context_label} action_gate={:?} target_gates={target_gate_count_after}",
                action_gate_after
            );
        }
    }
}

/// Apply a `ProjectNotify::Delete` to state. Removes the record, falls back
/// the active project if it was the deleted one (workbench → parent if
/// present, else home; standalone → home), and forgets the deleted project's
/// view-memory entry. Forget runs **after** the active switch so
/// `switch_active_project`'s outgoing-project snapshot can't reinsert it.
pub(crate) fn handle_project_delete(state: &AppState, host_id: &str, project: &protocol::Project) {
    state.projects.update(|projects| {
        projects.retain(|entry| !(entry.host_id == host_id && entry.project.id == project.id));
    });
    let deleted_ref = crate::state::ActiveProjectRef {
        host_id: host_id.to_string(),
        project_id: project.id.clone(),
    };
    if state
        .active_project
        .get_untracked()
        .as_ref()
        .is_some_and(|active| active == &deleted_ref)
    {
        // §7.4: when the active project is removed, fall back to its parent
        // (if a workbench whose parent is still present); otherwise fall
        // back to home.
        let fallback = project.parent_project_id().and_then(|parent_id| {
            let parent_id = parent_id.clone();
            let parent_present = state
                .projects
                .get_untracked()
                .into_iter()
                .any(|info| info.host_id == host_id && info.project.id == parent_id);
            parent_present.then(|| crate::state::ActiveProjectRef {
                host_id: host_id.to_string(),
                project_id: parent_id,
            })
        });
        state.switch_active_project(fallback);
    }
    state.forget_project_view_memory(&deleted_ref);
    // Drop per-project caches keyed by the deleted ProjectId so they can't
    // leak or be misread if the id ever reappears. Mirrors the mobile
    // dispatcher's delete handling. Workbench removals arrive as the same
    // Delete notification, so this covers both standalone project deletes
    // and workbench removals.
    let deleted_id = &project.id;
    state.file_tree.update(|map| {
        map.remove(deleted_id);
    });
    state.git_status.update(|map| {
        map.remove(deleted_id);
    });
    state.review_summaries.update(|map| {
        map.remove(deleted_id);
    });
    state.diff_contents.update(|map| {
        map.retain(|key, _| !(key.host_id == host_id && &key.project_id == deleted_id));
    });
}

fn apply_project_file_list(
    file_tree: &mut HashMap<ProjectId, Vec<protocol::ProjectRootListing>>,
    project_id: ProjectId,
    payload: ProjectFileListPayload,
) {
    if !payload.incremental {
        file_tree.insert(project_id, payload.roots);
        return;
    }

    let existing_roots = file_tree.entry(project_id).or_default();
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

        for entry in incoming_root.entries {
            match entry.op {
                protocol::FileEntryOp::Add => {
                    if !existing_root
                        .entries
                        .iter()
                        .any(|existing| existing.relative_path == entry.relative_path)
                    {
                        existing_root.entries.push(entry);
                    }
                }
                protocol::FileEntryOp::Remove => {
                    existing_root
                        .entries
                        .retain(|existing| existing.relative_path != entry.relative_path);
                }
            }
        }
        existing_root
            .entries
            .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    }
}

fn active_browse_dialog(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
) -> Option<crate::state::BrowseDialogState> {
    state.browse_dialog.with_untracked(|dialog| {
        dialog
            .as_ref()
            .filter(|d| {
                d.host_id.get_untracked() == host_id && d.browse_stream.get_untracked() == *stream
            })
            .cloned()
    })
}

fn dispatch_browse_opened(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseOpenedPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_opened on inactive stream {stream}");
        return;
    };
    dialog.platform.set(Some(payload.platform));
    dialog.separator.set(payload.separator);
    dialog.home.set(Some(payload.home));
}

fn dispatch_browse_entries(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseEntriesPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_entries on inactive stream {stream}");
        return;
    };
    dialog.error.set(None);
    dialog.current_path.set(Some(payload.path));
    dialog.parent.set(payload.parent);
    dialog.entries.set(payload.entries);
    dialog.loading.set(false);
}

fn dispatch_browse_error(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseErrorPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_error on inactive stream {stream}");
        return;
    };
    dialog.error.set(Some(payload));
    dialog.loading.set(false);
}

fn resolve_project_id(stream: &StreamPath) -> Option<ProjectId> {
    let suffix = stream.0.strip_prefix("/project/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ProjectId(suffix.to_string()))
}

/// Clear a host's inline workflow command error when a successful notify makes
/// it stale. Only clears when the stored error originated from one of `kinds`,
/// so a successful refresh notify does not erase a still-relevant trigger error
/// and vice versa.
fn clear_workflow_error_for_kinds(state: &AppState, host_id: &str, kinds: &[FrameKind]) {
    state.workflow_command_errors.update(|errors| {
        if errors
            .get(host_id)
            .is_some_and(|error| kinds.contains(&error.request_kind))
        {
            errors.remove(host_id);
        }
    });
}

/// Drop the optimistic-gate state any pending review-shaped command
/// was holding, scoped exactly to the command that failed.
///
/// `request_kind` distinguishes a `ReviewCreate` (the "Review changes"
/// button gate, keyed by host+project) from a `ReviewAction` (the
/// Submit/Cancel/AddComment/etc. gates, keyed by review id and target).
/// Anything else is ignored — those frames don't have a per-request UI
/// gate to release.
/// Release the native-settings save gate when a `SetSetting` command is rejected
/// by the server. A native save's result only arrives via a refreshed native
/// snapshot; a server-side rejection emits none, so any in-flight `Pending` save
/// would otherwise keep the native settings page disabled and stuck in
/// "Saving…". `CommandError` carries no backend/setting discriminator, so mark
/// every `Pending` native save on this host `Failed` with the server's message —
/// conservative and scoped to native settings (existing `Failed` entries and
/// non-native command errors are left untouched).
fn fail_native_settings_pending_on_error(
    state: &AppState,
    host_id: &str,
    payload: &CommandErrorPayload,
) {
    if !matches!(payload.request_kind, FrameKind::SetSetting) {
        return;
    }
    let has_pending = state.native_settings_save_state.with_untracked(|states| {
        states.get(host_id).is_some_and(|by_kind| {
            by_kind
                .values()
                .any(|save| matches!(save, NativeSettingsSaveState::Pending { .. }))
        })
    });
    if !has_pending {
        return;
    }
    state.native_settings_save_state.update(|states| {
        if let Some(by_kind) = states.get_mut(host_id) {
            for save in by_kind.values_mut() {
                if matches!(save, NativeSettingsSaveState::Pending { .. }) {
                    *save = NativeSettingsSaveState::Failed {
                        message: payload.message.clone(),
                    };
                }
            }
        }
    });
}

fn clear_review_pending_on_error(state: &AppState, host_id: &str, payload: &CommandErrorPayload) {
    match payload.request_kind {
        FrameKind::ReviewCreate => {
            let Some(project_id) = resolve_project_id(&payload.stream) else {
                log::warn!(
                    "command_error: ReviewCreate on non-project stream {}",
                    payload.stream
                );
                return;
            };
            let key = (host_id.to_string(), project_id.clone());
            let before = state
                .review_create_pending
                .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
            state.review_create_pending.update(|map| {
                if let Some(count) = map.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&key);
                    }
                }
            });
            let after = state
                .review_create_pending
                .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
            log::info!(
                "review.command_error.gate_clear host={host_id} project={project_id} kind=ReviewCreate create_pending_before={before} create_pending_after={after}"
            );
        }
        FrameKind::ReviewAction => {
            let Some(review_id) = resolve_review_id(&payload.stream) else {
                log::warn!(
                    "command_error: ReviewAction on non-review stream {}",
                    payload.stream
                );
                return;
            };
            let action_gate_before = state
                .review_action_pending
                .with_untracked(|m| m.get(&review_id).copied().unwrap_or_default());
            let target_gate_count_before = state
                .review_action_target_pending
                .with_untracked(|set| set.iter().filter(|(rid, _)| rid == &review_id).count());
            state.review_action_pending.update(|map| {
                map.remove(&review_id);
            });
            state.review_action_target_pending.update(|set| {
                set.retain(|(rid, _)| rid != &review_id);
            });
            log::info!(
                "review.command_error.gate_clear host={host_id} review={review_id} kind=ReviewAction action_gate_before={:?} target_gates_before={target_gate_count_before} action_gate_after={:?} target_gates_after=0",
                action_gate_before,
                crate::state::ReviewActionGate::default()
            );
        }
        _ => {}
    }
}

fn resolve_agent_id(state: &AppState, host_id: &str, stream: &StreamPath) -> Option<AgentId> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| agent.host_id == host_id && agent.instance_stream == *stream)
            .map(|agent| agent.agent_id.clone())
    })
}

fn apply_agent_started(
    state: &AppState,
    host_id: &str,
    agent_id: &AgentId,
    session_id: Option<SessionId>,
) {
    state.agents.update(|agents| {
        if let Some(agent) = agents
            .iter_mut()
            .find(|agent| agent.host_id == host_id && agent.agent_id == *agent_id)
        {
            agent.started = true;
            if session_id.is_some() {
                agent.session_id = session_id;
            }
        }
    });
}

fn apply_agent_activity_summary(
    state: &AppState,
    host_id: &str,
    payload: AgentActivitySummaryPayload,
) {
    let agent_id = payload.agent_id;
    log::info!(
        "dispatch agent_activity_summary host={host_id} agent_id={agent_id} state={:?}",
        payload.state
    );
    state.agents.update(|agents| {
        if let Some(agent) = agents
            .iter_mut()
            .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
        {
            agent.activity_summary = payload.state;
        } else {
            log::warn!("agent_activity_summary for unknown agent {agent_id} on host {host_id}");
        }
    });
}

fn apply_agent_activity_stats(state: &AppState, host_id: &str, payload: AgentActivityStatsPayload) {
    let agent_id = payload.agent_id;
    log::debug!(
        "dispatch agent_activity_stats host={host_id} agent_id={agent_id} tool_calls={}",
        payload.stats.tool_calls
    );
    state.agent_activity_stats.update(|map| {
        map.insert(
            ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id,
            },
            payload.stats,
        );
    });
}

fn apply_task_token_usage(state: &AppState, host_id: &str, payload: TaskTokenUsagePayload) {
    log::debug!(
        "dispatch task_token_usage host={} root_agent={} total_tokens={} descendants={}",
        host_id,
        payload.root_agent_id,
        payload.total.usage.total_tokens,
        payload.descendant_count
    );
    state.task_token_usage.update(|map| {
        map.insert(
            ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: payload.root_agent_id.clone(),
            },
            payload,
        );
    });
}

fn apply_agent_rename(state: &AppState, host_id: &str, payload: AgentRenamedPayload) {
    let agent_id = payload.agent_id;
    let name = payload.name;

    state.agents.update(|agents| {
        if let Some(agent) = agents
            .iter_mut()
            .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
        {
            agent.name = name.clone();
        }
    });

    state.streaming_text.update(|map| {
        if let Some(streaming) = map.get_mut(&agent_id) {
            streaming.agent_name = name.clone();
        }
    });

    state
        .center_zone
        .update(|cz| rename_agent_tabs(cz, host_id, &agent_id, &name));
    state.project_view_memory.update(|memories| {
        for memory in memories.values_mut() {
            if let Some(center_zone) = memory.center_zone.as_mut() {
                rename_agent_tabs(center_zone, host_id, &agent_id, &name);
            }
        }
    });
}

fn apply_agent_compact_notify(
    state: &AppState,
    host_id: &str,
    payload: protocol::types::AgentCompactNotifyPayload,
) {
    use protocol::types::AgentCompactStatus;
    log::info!(
        "dispatch agent_compact_notify host={} status={:?} old={} new={:?}",
        host_id,
        payload.status,
        payload.old_agent_id.0,
        payload.new_agent_id.as_ref().map(|a| a.0.as_str()),
    );
    match payload.status {
        AgentCompactStatus::Started => {
            state.mark_compaction_started(host_id, payload.old_agent_id);
        }
        AgentCompactStatus::Failed => {
            let message = payload
                .message
                .unwrap_or_else(|| "Compaction failed.".to_owned());
            state.finish_compaction_failure(payload.old_agent_id, message);
        }
        AgentCompactStatus::Completed => {
            let Some(new_agent_id) = payload.new_agent_id else {
                log::warn!(
                    "agent_compact_notify Completed without new_agent_id; clearing in-progress for {}",
                    payload.old_agent_id.0
                );
                state.compaction_in_progress.update(|map| {
                    map.remove(&payload.old_agent_id);
                });
                return;
            };
            // The replacement's `NewAgent` echo may have already landed
            // (typical) or it may still be in flight. If it's already in
            // `state.agents`, finalize the retarget immediately;
            // otherwise stash so the `NewAgent` arm flushes it.
            let new_info = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.host_id == host_id && a.agent_id == new_agent_id)
                    .cloned()
            });
            if let Some(new_info) = new_info {
                state.finish_compaction_success(&payload.old_agent_id, &new_info);
            } else {
                state.compaction_pending_completion.update(|map| {
                    map.insert((host_id.to_owned(), new_agent_id), payload.old_agent_id);
                });
            }
        }
    }
}

/// Per-agent compaction events for a team compact run reach the server
/// on internal team-compact streams, so the client never sees them as
/// `AgentCompactNotify` frames. Instead the server aggregates them into
/// `TeamCompactNotify.results`. Fan each per-agent result through the
/// same `apply_agent_compact_notify` path so retarget/finalize/error
/// state machines stay in one place. `Started` flags every targeted
/// agent as in-flight (idempotent if the local click handler already
/// marked them), so a team compact initiated by another client still
/// disables per-member affordances here.
fn apply_team_compact_notify(
    state: &AppState,
    host_id: &str,
    payload: protocol::types::TeamCompactNotifyPayload,
) {
    use protocol::types::TeamCompactStatus;
    log::info!(
        "dispatch team_compact_notify host={} team={} status={:?} agents={} results={}",
        host_id,
        payload.team_id,
        payload.status,
        payload.agent_ids.len(),
        payload.results.len(),
    );
    match payload.status {
        TeamCompactStatus::Started => {
            for agent_id in payload.agent_ids {
                state.mark_compaction_started(host_id, agent_id);
            }
        }
        TeamCompactStatus::Completed | TeamCompactStatus::Failed => {
            // Per-agent results: drive each through the same handler as
            // a solo agent compaction. Agents that succeeded retarget
            // their chat tab; agents that failed surface the inline
            // error and re-enable the compact button.
            let mut seen: std::collections::HashSet<AgentId> =
                std::collections::HashSet::with_capacity(payload.results.len());
            for result in payload.results {
                seen.insert(result.old_agent_id.clone());
                apply_agent_compact_notify(state, host_id, result);
            }
            // Defensive: any agent_id present in the Started fan-out but
            // missing from results would otherwise stay stuck in
            // `compaction_in_progress`. Treat as failure with the
            // team-level message so the UI re-enables.
            let team_message = payload.message.clone().unwrap_or_else(|| {
                "Team compaction did not report a result for this agent.".to_owned()
            });
            for agent_id in payload.agent_ids {
                if !seen.contains(&agent_id) {
                    let still_in_flight = state
                        .compaction_in_progress
                        .with_untracked(|map| map.contains_key(&agent_id));
                    if still_in_flight {
                        state.finish_compaction_failure(agent_id, team_message.clone());
                    }
                }
            }
        }
    }
}

fn apply_agent_closed(state: &AppState, host_id: &str, agent_id: AgentId) {
    // Defensive belt for ordering inversion. The current server
    // contract delivers compaction events in the order
    // `NewAgent (replacement) → Completed (on old, still-valid
    // stream) → AgentClosed (old)`, which means by the time we
    // reach here for a compaction old-agent close, `Completed`
    // has already cleared `compaction_in_progress` and the
    // branch below is skipped — we hit the normal teardown. If
    // the server ever inverts that ordering, defer teardown so
    // `state.agents`, `chat_rows`, and the user's chat tab stay
    // alive until `Completed` retargets the tab and
    // `finish_compaction_success` finalizes the close from the
    // `compaction_pending_close` set.
    let is_compacting = state
        .compaction_in_progress
        .with_untracked(|map| map.contains_key(&agent_id));
    if is_compacting {
        state.defer_compaction_close(host_id, agent_id);
        return;
    }
    state.agents.update(|agents| {
        agents.retain(|agent| !(agent.host_id == host_id && agent.agent_id == agent_id));
    });
    state.chat_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.chat_tool_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.tool_progress.update(|map| {
        map.retain(|(id, _), _| *id != agent_id);
    });
    state.chat_message_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.forget_session_history(&agent_id);
    state.streaming_text.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_activity_stats.update(|map| {
        map.remove(&ActiveAgentRef {
            host_id: host_id.to_owned(),
            agent_id: agent_id.clone(),
        });
    });
    state.task_token_usage.update(|map| {
        map.remove(&ActiveAgentRef {
            host_id: host_id.to_owned(),
            agent_id: agent_id.clone(),
        });
    });
    state.agent_turn_active.update(|map| {
        map.remove(&agent_id);
    });
    state.transient_events.update(|map| {
        map.remove(&agent_id);
    });
    state.task_lists.update(|map| {
        map.remove(&agent_id);
    });
    state.orchestration.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_message_queue.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_session_settings.update(|map| {
        map.remove(&agent_id);
    });

    // active_agent is a Memo over center_zone — closing the chat tabs below
    // drives it to None for this agent.
    state
        .center_zone
        .update(|cz| close_agent_tabs(cz, host_id, &agent_id));
    state.project_view_memory.update(|memories| {
        for memory in memories.values_mut() {
            if let Some(center_zone) = memory.center_zone.as_mut() {
                close_agent_tabs(center_zone, host_id, &agent_id);
            }
        }
    });
    // The center_zone update above can remove tab ids that are still in
    // `tab_lru`. Prune so we don't keep mounting references to vanished
    // tabs.
    state.prune_tab_lru();
}

/// Find a draft chat tab opened against `(host_id, member_id)` and replace its
/// content with a live `agent_ref`. Searches the current center zone first,
/// then each stored `project_view_memory` snapshot — a team click that
/// happened in another project's view still upgrades when the agent finally
/// spawns. Returns `true` if a tab was upgraded.
fn upgrade_pending_team_member_tab(
    state: &AppState,
    host_id: &str,
    member_id: &TeamMemberId,
    agent_ref: &ActiveAgentRef,
    agent_name: &str,
) -> bool {
    let matches_pending = |content: &TabContent| -> bool {
        matches!(
            content,
            TabContent::Chat {
                agent_ref: None,
                pending_team_member: Some(pending),
            } if pending.host_id == host_id && pending.member_id == *member_id
        )
    };

    let mut upgraded = false;
    state.center_zone.update(|cz| {
        if let Some(tab) = cz.tabs.iter_mut().find(|t| matches_pending(&t.content)) {
            tab.content = TabContent::chat_with_agent(agent_ref.clone());
            tab.label = agent_name.to_string();
            cz.active_tab_id = Some(tab.id);
            upgraded = true;
        }
    });
    if upgraded {
        return true;
    }
    state.project_view_memory.update(|map| {
        for memory in map.values_mut() {
            let Some(cz) = memory.center_zone.as_mut() else {
                continue;
            };
            if let Some(tab) = cz.tabs.iter_mut().find(|t| matches_pending(&t.content)) {
                tab.content = TabContent::chat_with_agent(agent_ref.clone());
                tab.label = agent_name.to_string();
                cz.active_tab_id = Some(tab.id);
                upgraded = true;
                break;
            }
        }
    });
    upgraded
}

fn close_agent_tabs(
    center_zone: &mut crate::state::CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
) {
    let remove_ids: Vec<_> = center_zone
        .tabs
        .iter()
        .filter(|tab| {
            matches!(
                &tab.content,
                TabContent::Chat { agent_ref: Some(ar), .. }
                    if ar.host_id == host_id && ar.agent_id == *agent_id
            )
        })
        .map(|tab| tab.id)
        .collect();
    for id in remove_ids {
        // Preserve non-closeable tabs (shouldn't exist for chats, but be safe).
        let closeable = center_zone
            .tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.closeable)
            .unwrap_or(true);
        if closeable {
            center_zone.close(id);
        }
    }
}

fn rename_agent_tabs(
    center_zone: &mut crate::state::CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
    name: &str,
) {
    for tab in &mut center_zone.tabs {
        let matches_agent = matches!(
            &tab.content,
            TabContent::Chat {
                agent_ref: Some(agent_ref),
                ..
            } if agent_ref.host_id == host_id && agent_ref.agent_id == *agent_id
        );
        if matches_agent {
            tab.label = name.to_string();
        }
    }
}

fn dispatch_chat_event(state: &AppState, host_id: &str, stream: &StreamPath, envelope: &Envelope) {
    let Some(agent_id) = resolve_agent_id(state, host_id, stream) else {
        log::warn!("chat_event on unknown stream {stream}");
        return;
    };

    let event = match envelope.parse_payload::<ChatEvent>() {
        Ok(event) => event,
        Err(error) => {
            log::error!(
                "failed to parse chat_event payload: {error}\nraw: {}",
                serde_json::to_string(&envelope.payload).unwrap_or_default(),
            );
            return;
        }
    };

    apply_chat_event(state, host_id, &agent_id, event);
}

fn clear_session_history_loading_on_error(
    state: &AppState,
    host_id: &str,
    payload: &CommandErrorPayload,
) {
    if !matches!(payload.request_kind, FrameKind::FetchSessionHistory) {
        return;
    }
    let Some(agent_id) = resolve_agent_id(state, host_id, &payload.stream) else {
        log::warn!(
            "fetch_session_history error on unknown stream host={} stream={}",
            host_id,
            payload.stream
        );
        return;
    };
    state.session_history.update(|map| {
        if let Some(history) = map.get_mut(&agent_id) {
            history.loading = false;
        }
    });
}

fn clear_session_history_loading_for_host(state: &AppState, host_id: &str) {
    let agent_ids = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .filter(|agent| agent.host_id == host_id)
            .map(|agent| agent.agent_id.clone())
            .collect::<HashSet<_>>()
    });
    state.session_history.update(|map| {
        for agent_id in agent_ids {
            if let Some(history) = map.get_mut(&agent_id) {
                history.loading = false;
            }
        }
    });
}

fn apply_session_history(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: SessionHistoryPayload,
) {
    let Some(agent_id) = resolve_agent_id(state, host_id, stream) else {
        log::warn!("session_history on unknown stream {stream}");
        return;
    };
    if payload.agent_id != agent_id {
        log::error!(
            "session_history agent mismatch host={} stream={} payload_agent_id={} stream_agent_id={}",
            host_id,
            stream,
            payload.agent_id,
            agent_id
        );
        return;
    }

    let mut replay = HistoryReplay::default();
    for event in payload.events.into_iter().rev() {
        replay.apply(event, host_id, &agent_id);
    }

    if !replay.rows.is_empty() {
        state.chat_rows.update(|map| {
            let current = map.remove(&agent_id).unwrap_or_default();
            let mut combined = replay.rows.clone();
            combined.extend(current);
            map.insert(agent_id.clone(), combined);
        });
        state.chat_message_rows.update(|map| {
            let agent_index = map.entry(agent_id.clone()).or_default();
            for (message_id, row_id) in replay.message_rows {
                agent_index.insert(message_id, row_id);
            }
        });
        state.chat_tool_rows.update(|map| {
            let agent_index = map.entry(agent_id.clone()).or_default();
            for (tool_call_id, row_id) in replay.tool_rows {
                agent_index.insert(tool_call_id, row_id);
            }
        });
    }

    for (tool_call_id, progress) in replay.tool_progress {
        let key = (agent_id.clone(), tool_call_id);
        let existing = state
            .tool_progress
            .with_untracked(|map| map.get(&key).cloned());
        if let Some(signal) = existing {
            signal.set(progress);
        } else {
            state.tool_progress.update(|map| {
                map.insert(key, leptos::prelude::ArcRwSignal::new(progress));
            });
        }
    }

    state.session_history.update(|map| {
        let mut remove = false;
        if let Some(history) = map.get_mut(&agent_id) {
            history.oldest_seq = payload.oldest_seq;
            history.has_more_before = payload.has_more_before;
            history.loading = false;
            history.message_count = 0;
            if !history.has_more_before {
                remove = true;
            }
        } else if payload.has_more_before {
            map.insert(
                agent_id.clone(),
                SessionHistoryState {
                    message_count: 0,
                    oldest_seq: payload.oldest_seq,
                    has_more_before: payload.has_more_before,
                    loading: false,
                },
            );
        }
        if remove {
            map.remove(&agent_id);
        }
    });
}

/// True for the root orchestration announcement (`AgentStarted` with a `Root`
/// origin, or the depth-1 interactive agent). The root persists across turns —
/// it is announced once per session — so it is retained when a new user turn
/// prunes the per-turn orchestration log, keeping the panel's workflow header.
fn is_root_orchestration_record(record: &OrchestrationRecord) -> bool {
    let OrchestrationRecord::Event(event) = record else {
        return false;
    };
    matches!(
        &event.payload,
        protocol::OrchestrationPayload::AgentStarted { origin, depth, .. }
            if matches!(origin, protocol::OrchestrationAgentOrigin::Root) || *depth <= 1
    )
}

#[derive(Default)]
struct HistoryReplay {
    rows: Vec<crate::state::ChatRowHandle>,
    message_rows: HashMap<protocol::ChatMessageId, crate::state::ChatRowId>,
    tool_rows: HashMap<ToolCallId, crate::state::ChatRowId>,
    tool_progress: HashMap<ToolCallId, protocol::ToolProgressData>,
}

impl HistoryReplay {
    fn apply(&mut self, event: ChatEvent, host_id: &str, agent_id: &AgentId) {
        match event {
            ChatEvent::MessageAdded(message) => {
                self.push_entry(ChatMessageEntry {
                    message,
                    tool_requests: Vec::new(),
                });
            }
            ChatEvent::MessageMetadataUpdated(data) => {
                let Some(row_id) = self.message_rows.get(&data.message_id).copied() else {
                    return;
                };
                let Some(row) = self.rows.iter().find(|row| row.id == row_id) else {
                    return;
                };
                row.entry.update(|entry| {
                    if data.model_info.is_some() {
                        entry.message.model_info = data.model_info.clone();
                    }
                    if data.token_usage.is_some() {
                        entry.message.token_usage = data.token_usage.clone();
                    }
                    if data.context_breakdown.is_some() {
                        entry.message.context_breakdown = data.context_breakdown.clone();
                    }
                });
            }
            ChatEvent::StreamEnd(data) => {
                if history_message_has_renderable_content(&data.message) {
                    self.push_entry(ChatMessageEntry {
                        message: data.message,
                        tool_requests: Vec::new(),
                    });
                }
            }
            ChatEvent::ToolRequest(request) => {
                let tool_name = request.tool_name.clone();
                let tool_call_id = request.tool_call_id.clone();
                let tool_entry = ToolRequestEntry {
                    request,
                    result: None,
                };
                if let Some(row) = self.rows.last() {
                    row.entry.update(|entry| {
                        entry.tool_requests.push(tool_entry);
                    });
                    self.tool_rows.insert(ToolCallId(tool_call_id), row.id);
                } else {
                    log::error!(
                        "HISTORY TOOL REQUEST DROPPED: tool '{}' (call_id={}) for host {} agent {} — history page has no message row",
                        tool_name,
                        tool_call_id,
                        host_id,
                        agent_id
                    );
                }
            }
            ChatEvent::ToolExecutionCompleted(data) => {
                let call_id = data.tool_call_id.clone();
                let Some(row_id) = self.tool_rows.get(&ToolCallId(call_id.clone())).copied() else {
                    return;
                };
                let Some(row) = self.rows.iter().find(|row| row.id == row_id) else {
                    return;
                };
                row.entry.update(|entry| {
                    if let Some(tool) = entry
                        .tool_requests
                        .iter_mut()
                        .find(|tool| tool.request.tool_call_id == call_id)
                    {
                        tool.result = Some(data);
                    }
                });
            }
            ChatEvent::ToolProgress(data) => {
                self.tool_progress
                    .insert(ToolCallId(data.tool_call_id.clone()), data);
            }
            // Paged "load earlier messages" history is strictly older turns.
            // Orchestration is scoped to the current turn (a live session would
            // have already pruned these on each subsequent user message), so
            // replaying it here would resurrect stale prior-turn workers. Leave
            // orchestration untouched; the current turn's panel comes from the
            // live stream and the authoritative agent bootstrap.
            ChatEvent::TypingStatusChanged(_)
            | ChatEvent::StreamStart(_)
            | ChatEvent::StreamDelta(_)
            | ChatEvent::StreamReasoningDelta(_)
            | ChatEvent::TaskUpdate(_)
            | ChatEvent::OperationCancelled(_)
            | ChatEvent::RetryAttempt(_)
            | ChatEvent::Orchestration(_) => {}
        }
    }

    fn push_entry(&mut self, entry: ChatMessageEntry) {
        let handle = crate::state::ChatRowHandle::new(entry);
        let (tool_call_ids, message_id) = handle.entry.with_untracked(|entry| {
            (
                entry
                    .tool_requests
                    .iter()
                    .map(|tool| tool.request.tool_call_id.clone())
                    .collect::<Vec<_>>(),
                entry.message.message_id.clone(),
            )
        });
        if let Some(message_id) = message_id {
            self.message_rows.insert(message_id, handle.id);
        }
        for tool_call_id in tool_call_ids {
            self.tool_rows.insert(ToolCallId(tool_call_id), handle.id);
        }
        self.rows.push(handle);
    }
}

fn history_message_has_renderable_content(message: &protocol::ChatMessage) -> bool {
    !message.content.trim().is_empty()
        || message
            .reasoning
            .as_ref()
            .is_some_and(|reasoning| !reasoning.text.trim().is_empty())
        || !message.tool_calls.is_empty()
        || message
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
}

/// Apply an already-parsed `ChatEvent` to the per-agent state.
///
/// Split out from `dispatch_chat_event` so an `AgentBootstrap` (or any
/// future code path that already holds a parsed event) can replay inner
/// events through the same reducer without re-encoding them through an
/// `Envelope`.
pub fn apply_chat_event(state: &AppState, host_id: &str, agent_id: &AgentId, event: ChatEvent) {
    let agent_id = agent_id.clone();
    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=typing active={}",
                host_id,
                agent_id,
                typing
            );
            if typing {
                state.transient_events.update(|events| {
                    events.remove(&agent_id);
                });
            }
            state.agent_turn_active.update(|map| {
                if typing {
                    map.insert(agent_id.clone(), true);
                } else {
                    map.remove(&agent_id);
                }
            });
        }
        ChatEvent::MessageAdded(message) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=message_added sender={:?} text_len={}",
                host_id,
                agent_id,
                message.sender,
                message.content.len()
            );
            // A user message opens a new turn. Tycode announces the root
            // orchestration agent once for the session, but its fan-outs,
            // phases, and workers are per-turn — so drop the previous turn's
            // orchestration (retaining only the root announcement) rather than
            // letting the panel grow unbounded and show stale prior-turn
            // workers as if current. Bootstrap replay runs through this same
            // reducer, so replayed history segments identically.
            if matches!(message.sender, protocol::MessageSender::User) {
                state.orchestration.update(|map| {
                    if let Some(log) = map.get_mut(&agent_id) {
                        log.retain(is_root_orchestration_record);
                    }
                });
            }
            let entry = ChatMessageEntry {
                message,
                tool_requests: Vec::new(),
            };
            state.push_chat_entry(agent_id.clone(), entry);
        }
        ChatEvent::MessageMetadataUpdated(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=message_metadata_updated message_id={}",
                host_id,
                agent_id,
                data.message_id
            );
            state.apply_chat_message_metadata(&agent_id, data);
        }
        ChatEvent::StreamStart(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=stream_start message_id={:?} model={:?}",
                host_id,
                agent_id,
                data.message_id,
                data.model
            );
            state.transient_events.update(|events| {
                events.remove(&agent_id);
            });
            let streaming = StreamingState {
                agent_name: data.agent,
                model: data.model,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            };
            state.streaming_text.update(|map| {
                map.insert(agent_id.clone(), streaming);
            });
        }
        ChatEvent::StreamDelta(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=stream_delta message_id={:?} text_len={}",
                host_id,
                agent_id,
                data.message_id,
                data.text.len()
            );
            // Pull out only the text-signal handle, not the entire
            // StreamingState — cloning the latter copies the
            // `agent_name`/`model` Strings on every delta. With ~50
            // deltas/sec from a fast model that's a steady drip of
            // small allocations the GC has to manage.
            let text_signal = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).map(|s| s.text.clone()));
            if let Some(text_signal) = text_signal {
                text_signal.update(|text| text.push_str(&data.text));
            }
        }
        ChatEvent::StreamReasoningDelta(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=reasoning_delta message_id={:?} text_len={}",
                host_id,
                agent_id,
                data.message_id,
                data.text.len()
            );
            let reasoning_signal = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).map(|s| s.reasoning.clone()));
            if let Some(reasoning_signal) = reasoning_signal {
                reasoning_signal.update(|reasoning| reasoning.push_str(&data.text));
            }
        }
        ChatEvent::StreamEnd(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=stream_end text_len={} tool_calls={}",
                host_id,
                agent_id,
                data.message.content.len(),
                data.message.tool_calls.len()
            );
            // Read the stream's tool_requests without cloning the
            // surrounding StreamingState (which carries `agent_name`
            // and `model` strings we don't need here).
            let tool_requests = state
                .streaming_text
                .with_untracked(|map| {
                    map.get(&agent_id).map(|s| {
                        s.tool_requests.with_untracked(|tools| {
                            tools
                                .iter()
                                .map(|tool| tool.entry.get_untracked())
                                .collect::<Vec<_>>()
                        })
                    })
                })
                .unwrap_or_default();
            state.streaming_text.update(|map| {
                map.remove(&agent_id);
            });
            let has_renderable_content = !data.message.content.trim().is_empty()
                || data
                    .message
                    .reasoning
                    .as_ref()
                    .is_some_and(|reasoning| !reasoning.text.trim().is_empty())
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
            state.push_chat_entry(agent_id.clone(), entry);
        }
        ChatEvent::ToolRequest(request) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=tool_request tool_call_id={} tool_name={}",
                host_id,
                agent_id,
                request.tool_call_id,
                request.tool_name
            );
            let tool_name = request.tool_name.clone();
            let tool_call_id = request.tool_call_id.clone();
            let tool_entry = ToolRequestEntry {
                request,
                result: None,
            };
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                streaming.tool_requests.update(|tools| {
                    tools.push(StreamingToolRequest {
                        tool_call_id: tool_call_id.clone(),
                        entry: leptos::prelude::ArcRwSignal::new(tool_entry),
                    });
                });
                return;
            }
            if let Some(row) = state.last_chat_row_untracked(&agent_id) {
                row.entry.update(|entry| {
                    entry.tool_requests.push(tool_entry);
                });
                state.index_chat_tool_row(&agent_id, tool_call_id, row.id);
            } else {
                log::error!(
                    "TOOL REQUEST DROPPED: tool '{}' (call_id={}) for host {} agent {} — agent has no message row",
                    tool_name,
                    tool_call_id,
                    host_id,
                    agent_id
                );
            }
        }
        ChatEvent::ToolExecutionCompleted(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=tool_execution_completed tool_call_id={} tool_name={} success={}",
                host_id,
                agent_id,
                data.tool_call_id,
                data.tool_name,
                data.success
            );
            let call_id = data.tool_call_id.clone();
            let tool_name = data.tool_name.clone();
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                let tool_entry = streaming.tool_requests.with_untracked(|tools| {
                    tools
                        .iter()
                        .find(|tool| tool.tool_call_id == call_id)
                        .map(|tool| tool.entry.clone())
                });
                if let Some(tool_entry) = tool_entry {
                    tool_entry.update(|tool| {
                        tool.result = Some(data.clone());
                    });
                    return;
                }
            }
            if let Some(row) = state.chat_row_for_tool_untracked(&agent_id, &call_id) {
                let mut matched = false;
                row.entry.update(|entry| {
                    if let Some(tool) = entry
                        .tool_requests
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
            log::error!(
                "TOOL RESULT ORPHANED: completion for tool '{}' (call_id={}) for host {} agent {} — no matching request found",
                tool_name,
                call_id,
                host_id,
                agent_id
            );
        }
        ChatEvent::ToolProgress(data) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=tool_progress tool_call_id={} tool_name={}",
                host_id,
                agent_id,
                data.tool_call_id,
                data.tool_name
            );
            // Single store: tool cards and the workflow tab read this map
            // reactively by (agent, tool_call_id). No row mutation — a
            // snapshot stored on the entry would freeze inside keyed
            // `<For>` rows, and progress may arrive before its tool
            // request or after the turn ends; the map is indifferent to
            // both.
            let key = (agent_id, ToolCallId(data.tool_call_id.clone()));
            let existing = state
                .tool_progress
                .with_untracked(|map| map.get(&key).cloned());
            if let Some(signal) = existing {
                signal.set(data);
            } else {
                state.tool_progress.update(|map| {
                    map.insert(key, leptos::prelude::ArcRwSignal::new(data));
                });
            }
        }
        ChatEvent::TaskUpdate(task_list) => {
            log::trace!(
                "dispatch chat_event host={} agent_id={} type=task_update items={}",
                host_id,
                agent_id,
                task_list.tasks.len()
            );
            state.task_lists.update(|task_lists| {
                task_lists.insert(agent_id.clone(), task_list);
            });
        }
        ChatEvent::OperationCancelled(data) => {
            log::warn!(
                "dispatch chat_event host={} agent_id={} type=operation_cancelled message={}",
                host_id,
                agent_id,
                data.message
            );
            state.streaming_text.update(|map| {
                map.remove(&agent_id);
            });
            // Close any in-flight orchestration: Tycode drops fan-outs and
            // workers without terminal events on cancel, so the panel would
            // otherwise show them stuck "running".
            state.orchestration.update(|map| {
                if let Some(log) = map.get_mut(&agent_id) {
                    log.push(OrchestrationRecord::Cancelled);
                }
            });
            state.transient_events.update(|events| {
                events.entry(agent_id.clone()).or_default().push(
                    TransientEvent::OperationCancelled {
                        message: data.message,
                    },
                );
            });
        }
        ChatEvent::RetryAttempt(data) => {
            log::warn!(
                "dispatch chat_event host={} agent_id={} type=retry attempt={} max_retries={} backoff_ms={} error={}",
                host_id,
                agent_id,
                data.attempt,
                data.max_retries,
                data.backoff_ms,
                data.error
            );
            state.transient_events.update(|events| {
                events
                    .entry(agent_id)
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
                "dispatch chat_event host={} agent_id={} type=orchestration orchestration_agent_id={} orchestration_agent_type={} payload={}",
                host_id,
                agent_id,
                data.agent_id,
                data.agent_type,
                data.payload.kind()
            );
            state.orchestration.update(|map| {
                map.entry(agent_id.clone())
                    .or_default()
                    .push(OrchestrationRecord::Event(data));
            });
        }
    }
}

// ── Bootstrap apply helpers ──────────────────────────────────────────────
//
// HostBootstrap (seq 1 on the host stream after Welcome at seq 0) carries
// a full snapshot of host-scoped state that previously arrived as a flurry
// of independent notify frames. The apply helpers below replace each
// host-keyed slice in `AppState` with the snapshot. They must not have
// side effects that depend on user intent (e.g. opening chat tabs,
// stealing focus) — those live in the per-event arms.

/// Apply a `MobileAccessStatePayload` to per-host state.
///
/// Used by the live `MobileAccessState` arm and by `apply_host_bootstrap`,
/// so a bootstrap that arrives while the previous connection's stored
/// pairing offer is stale gets the same reconciliation as a live update.
///
/// Reconciliation rules:
///   1. Phase not `Active` → drop any stored pairing offer (Consumed /
///      Expired / Cancelled / Failed / Idle should render no QR).
///   2. Phase `Active { offer_id: NEW }` but stored offer's id is
///      different → drop the stored offer. The server may broadcast a
///      new `Active` to bystanders without re-sending the matching
///      `MobilePairingOffer` (only the requester gets the offer
///      payload). A bystander would otherwise keep rendering the stale
///      QR and Cancel would target the wrong id.
fn apply_mobile_access_state(state: &AppState, host_id: &str, payload: MobileAccessStatePayload) {
    // Don't log the QR uri — pairing payloads are log-sensitive. The
    // Display impl on `MobilePairingState` is structural so this is
    // safe.
    log::info!(
        "dispatch mobile_access_state host={} pairing={:?} paired_devices={}",
        host_id,
        std::mem::discriminant(&payload.pairing),
        payload.paired_devices.len()
    );
    // Any non-Idle state means the server received our Start (or
    // someone else's), so the start-pending gate can clear.
    if !matches!(payload.pairing, MobilePairingState::Idle) {
        state.mobile_pairing_start_pending.update(|set| {
            set.remove(host_id);
        });
    }
    let drop_offer = match &payload.pairing {
        MobilePairingState::Active { offer_id, .. } => {
            state.mobile_pairing_offer.with_untracked(|m| {
                m.get(host_id)
                    .map(|stored| &stored.offer_id != offer_id)
                    .unwrap_or(false)
            })
        }
        _ => true,
    };
    if drop_offer {
        state.mobile_pairing_offer.update(|m| {
            m.remove(host_id);
        });
    }
    state.mobile_access_state.update(|m| {
        m.insert(host_id.to_string(), payload);
    });
}

fn apply_host_bootstrap(state: &AppState, host_id: &str, payload: HostBootstrapPayload) {
    log::info!(
        "dispatch host_bootstrap host={} sessions={} projects={} agents={} teams={} team_members={}",
        host_id,
        payload.sessions.len(),
        payload.projects.len(),
        payload.agents.len(),
        payload.teams.len(),
        payload.team_members.len(),
    );

    state.host_settings_by_host.update(|map| {
        map.insert(host_id.to_string(), payload.settings);
    });
    // Only the primary local host emits `Some` Agents-view preferences. Remote
    // hosts send `None` and must not clobber the client-global snapshot.
    if let Some(snapshot) = payload.agents_view_preferences {
        state.apply_agents_view_snapshot(host_id, snapshot);
    }
    // Route mobile access through the shared reconciler so a stale
    // pairing offer from a previous connection is dropped when the
    // bootstrap's pairing state no longer matches.
    apply_mobile_access_state(state, host_id, payload.mobile_access);
    state.backend_setup_by_host.update(|map| {
        map.insert(host_id.to_string(), payload.backend_setup.backends);
    });
    state.session_schemas.update(|schemas_by_host| {
        let host_schemas = schemas_by_host.entry(host_id.to_string()).or_default();
        host_schemas.clear();
        for schema in payload.session_schemas {
            host_schemas.insert(schema.backend_kind(), schema);
        }
    });
    state.schemas_loaded_for_host.update(|loaded| {
        loaded.insert(host_id.to_string(), true);
    });
    state.launch_profile_catalog.update(|map| {
        map.insert(host_id.to_string(), payload.launch_profile_catalog);
    });
    state.backend_config_schemas.update(|schemas_by_host| {
        let host_schemas = schemas_by_host.entry(host_id.to_string()).or_default();
        host_schemas.clear();
        for schema in payload.backend_config_schemas {
            host_schemas.insert(schema.backend_kind, schema);
        }
    });
    state.backend_config_snapshots.update(|snapshots_by_host| {
        let host_snapshots = snapshots_by_host.entry(host_id.to_string()).or_default();
        host_snapshots.clear();
        for snapshot in payload.backend_config_snapshots {
            host_snapshots.insert(snapshot.backend_kind, snapshot);
        }
    });
    // Bootstrap doesn't carry backend-native settings (they arrive via a later
    // BackendConfigSnapshots frame once probed). Clear any stale entries from a
    // prior connection so the native settings page never shows old values before
    // the follow-up snapshot lands.
    state.backend_native_settings.update(|snapshots_by_host| {
        if let Some(host_snapshots) = snapshots_by_host.get_mut(host_id) {
            host_snapshots.clear();
        }
    });
    state.native_settings_save_state.update(|states_by_host| {
        states_by_host.remove(host_id);
    });
    state.sessions.update(|sessions| {
        sessions.retain(|session| session.host_id != host_id);
        sessions.extend(payload.sessions.into_iter().map(|summary| SessionInfo {
            host_id: host_id.to_string(),
            summary,
        }));
        sessions.sort_by_key(|session| std::cmp::Reverse(session.summary.updated_at_ms));
    });
    state.projects.update(|projects| {
        projects.retain(|entry| entry.host_id != host_id);
        projects.extend(payload.projects.into_iter().map(|project| ProjectInfo {
            host_id: host_id.to_string(),
            project,
        }));
        sort_project_infos(projects);
    });
    state.mcp_servers.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for mcp_server in payload.mcp_servers {
            host_map.insert(mcp_server.id.clone(), mcp_server);
        }
    });
    state.skills.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for skill in payload.skills {
            host_map.insert(skill.id.clone(), skill);
        }
    });
    state.steering.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for steering in payload.steering {
            host_map.insert(steering.id.clone(), steering);
        }
    });
    state.custom_agents.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for custom_agent in payload.custom_agents {
            host_map.insert(custom_agent.id.clone(), custom_agent);
        }
    });
    state.task_token_usage.update(|map| {
        map.retain(|key, _| key.host_id != host_id);
        for usage in payload.task_token_usages {
            map.insert(
                ActiveAgentRef {
                    host_id: host_id.to_string(),
                    agent_id: usage.root_agent_id.clone(),
                },
                usage,
            );
        }
    });
    state.workflow_summaries.update(|map| {
        map.insert(host_id.to_string(), payload.workflow_summaries);
    });
    state.workflow_diagnostics.update(|map| {
        map.insert(host_id.to_string(), payload.workflow_diagnostics);
    });
    state.workflow_runs.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for run in payload.workflow_runs {
            host_map.insert(run.id.clone(), run);
        }
    });
    state.workflow_locations.update(|map| {
        map.insert(host_id.to_string(), payload.workflow_locations);
    });
    state.team_preset_catalogs.update(|map| {
        map.insert(host_id.to_string(), payload.team_preset_catalog);
    });
    state.team_drafts.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for draft in payload.team_drafts {
            host_map.insert(draft.id.clone(), draft);
        }
    });
    state.teams.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for team in payload.teams {
            host_map.insert(team.id.clone(), team);
        }
    });
    state.team_members.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for member in payload.team_members {
            host_map.insert(member.id.clone(), member);
        }
    });
    state.team_member_bindings.update(|map| {
        let host_map = map.entry(host_id.to_string()).or_default();
        host_map.clear();
        for binding in payload.team_member_bindings {
            host_map.insert(binding.member_id.clone(), binding);
        }
    });
    // Drop any agents the snapshot doesn't know about and upsert the rest
    // without opening tabs or stealing focus. The live `NewAgent` arm is
    // the only place that runs the auto-tab / compaction side effects.
    let snapshot_ids: HashSet<AgentId> =
        payload.agents.iter().map(|p| p.agent_id.clone()).collect();
    // Prune prior-history state for agents on this host the snapshot no
    // longer knows about.
    let dropped_ids: Vec<AgentId> = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .filter(|agent| agent.host_id == host_id && !snapshot_ids.contains(&agent.agent_id))
            .map(|agent| agent.agent_id.clone())
            .collect()
    });
    for dropped in &dropped_ids {
        state.forget_session_history(dropped);
    }
    state.agents.update(|agents| {
        agents.retain(|agent| agent.host_id != host_id || snapshot_ids.contains(&agent.agent_id));
        for payload in payload.agents {
            let mut info = agent_info_from_payload(host_id, payload);
            if let Some(existing) = agents
                .iter_mut()
                .find(|a| a.host_id == host_id && a.agent_id == info.agent_id)
            {
                // `agent_info_from_payload` zeroes runtime-only fields
                // (`started`, `fatal_error`) because `NewAgentPayload`
                // doesn't carry them. It may also omit `session_id`
                // before backend startup completes. Preserve whatever the live event
                // stream had set on the existing entry so a bootstrap
                // re-application doesn't reset an already-started agent
                // to `started: false`.
                info.started = existing.started;
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

fn agent_info_from_payload(host_id: &str, payload: NewAgentPayload) -> AgentInfo {
    AgentInfo {
        host_id: host_id.to_string(),
        agent_id: payload.agent_id,
        name: payload.name,
        origin: payload.origin,
        backend_kind: payload.backend_kind,
        workspace_roots: payload.workspace_roots,
        project_id: payload.project_id,
        parent_agent_id: payload.parent_agent_id,
        session_id: payload.session_id,
        custom_agent_id: payload.custom_agent_id,
        workflow: payload.workflow,
        created_at_ms: payload.created_at_ms,
        instance_stream: payload.instance_stream,
        started: false,
        fatal_error: None,
        activity_summary: payload.activity_summary,
    }
}

fn apply_agent_bootstrap(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: AgentBootstrapPayload,
) {
    let Some(agent_id) = resolve_agent_id(state, host_id, stream) else {
        log::warn!("agent_bootstrap on unknown stream {stream}");
        return;
    };
    log::info!(
        "dispatch agent_bootstrap host={} stream={} agent_id={} events={}",
        host_id,
        stream,
        agent_id,
        payload.events.len()
    );
    // Replace prior chat/stream/queue/task state for this agent so the
    // bootstrap snapshot is authoritative. Any inner events below replay
    // back into these now-clean slots.
    state.chat_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.chat_tool_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.tool_progress.update(|map| {
        map.retain(|(id, _), _| *id != agent_id);
    });
    state.chat_message_rows.update(|map| {
        map.remove(&agent_id);
    });
    state.forget_session_history(&agent_id);
    state.streaming_text.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_activity_stats.update(|map| {
        map.remove(&ActiveAgentRef {
            host_id: host_id.to_owned(),
            agent_id: agent_id.clone(),
        });
    });
    state.agent_turn_active.update(|map| {
        map.remove(&agent_id);
    });
    state.transient_events.update(|map| {
        map.remove(&agent_id);
    });
    state.task_lists.update(|map| {
        map.remove(&agent_id);
    });
    // Bootstrap is authoritative history replacement: drop prior orchestration
    // so the replayed ChatEvents below rebuild the current turn cleanly.
    state.orchestration.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_message_queue.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_session_settings.update(|map| {
        map.remove(&agent_id);
    });

    for event in payload.events {
        match event {
            AgentBootstrapEvent::AgentStart(inner) => {
                apply_agent_started(state, host_id, &agent_id, inner.session_id);
            }
            AgentBootstrapEvent::AgentError(inner) => {
                if inner.fatal {
                    state.agents.update(|agents| {
                        if let Some(agent) = agents
                            .iter_mut()
                            .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
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
                state.push_chat_entry(agent_id.clone(), entry);
            }
            AgentBootstrapEvent::SessionSettings(inner) => {
                state.agent_session_settings.update(|map| {
                    map.insert(agent_id.clone(), inner.values);
                });
            }
            AgentBootstrapEvent::QueuedMessages(inner) => {
                state.agent_message_queue.update(|map| {
                    map.insert(agent_id.clone(), inner.messages);
                });
            }
            AgentBootstrapEvent::HasPriorHistory {
                message_count,
                before_seq,
            } => {
                if message_count > 0 {
                    state.session_history.update(|map| {
                        map.insert(
                            agent_id.clone(),
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
                apply_chat_event(state, host_id, &agent_id, event);
            }
            AgentBootstrapEvent::AgentActivityStats(inner) => {
                apply_agent_activity_stats(state, host_id, inner);
            }
        }
    }
}

fn apply_project_bootstrap(
    state: &AppState,
    host_id: &str,
    project_id: ProjectId,
    payload: ProjectBootstrapPayload,
) {
    log::info!(
        "dispatch project_bootstrap host={} project_id={} reviews={}",
        host_id,
        project_id,
        payload.review_summaries.len()
    );

    state.projects.update(|projects| {
        if let Some(existing) = projects
            .iter_mut()
            .find(|entry| entry.host_id == host_id && entry.project.id == payload.project.id)
        {
            existing.project = payload.project;
        } else {
            projects.push(ProjectInfo {
                host_id: host_id.to_string(),
                project: payload.project,
            });
            sort_project_infos(projects);
        }
    });

    state.file_tree.update(|file_tree| {
        // The bootstrap file_list is a full snapshot. Force non-incremental
        // apply so we replace the per-root listing rather than merging.
        let full_payload = ProjectFileListPayload {
            incremental: false,
            roots: payload.file_list.roots,
        };
        apply_project_file_list(file_tree, project_id.clone(), full_payload);
    });
    state.git_status.update(|git_status| {
        git_status.insert(project_id.clone(), payload.git_status.roots);
    });
    state.review_summaries.update(|map| {
        map.insert(project_id, payload.review_summaries);
    });
}

fn apply_review_bootstrap(state: &AppState, review_id: &ReviewId, payload: ReviewBootstrapPayload) {
    log::info!(
        "dispatch review_bootstrap review={} comments={} suggestions={} diffs={}",
        review_id,
        payload.review.comments.len(),
        payload.review.suggestions.len(),
        payload.review.diffs.len(),
    );
    apply_review_event(
        state,
        review_id,
        ReviewEventPayload::Snapshot {
            review: payload.review,
        },
    );
}

fn apply_browse_bootstrap(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: BrowseBootstrapPayload,
) {
    log::info!(
        "dispatch browse_bootstrap host={} stream={}",
        host_id,
        stream
    );
    dispatch_browse_opened(state, host_id, stream, payload.opened);
    match payload.listing {
        BrowseBootstrapListing::Entries { entries } => {
            dispatch_browse_entries(state, host_id, stream, entries);
        }
        BrowseBootstrapListing::Error { error } => {
            dispatch_browse_error(state, host_id, stream, error);
        }
    }
}

fn apply_terminal_bootstrap(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: TerminalBootstrapPayload,
) {
    log::info!(
        "dispatch terminal_bootstrap host={} stream={} terminal_id={}",
        host_id,
        stream,
        payload.terminal_id
    );
    // Upsert the terminal: the NewTerminal arm normally creates the row,
    // but a bootstrap means the terminal already exists server-side and
    // this client just (re)subscribed. Avoid duplicate rows on resub.
    state.terminals.update(|terminals| {
        let exists = terminals
            .iter()
            .any(|t| t.host_id == host_id && t.terminal_id == payload.terminal_id);
        if !exists {
            terminals.push(TerminalInfo {
                host_id: host_id.to_string(),
                terminal_id: payload.terminal_id.clone(),
                stream: stream.clone(),
                project_id: payload.start.project_id.clone(),
                root: payload.start.root.clone(),
                cwd: payload.start.cwd.clone(),
                shell: payload.start.shell.clone(),
                cols: payload.start.cols,
                rows: payload.start.rows,
                created_at_ms: payload.start.created_at_ms,
                pending_output: Vec::new(),
                widget_mounted: false,
                exited: false,
                exit_code: None,
                exit_signal: None,
            });
            return;
        }
        if let Some(terminal) = terminals
            .iter_mut()
            .find(|t| t.host_id == host_id && t.terminal_id == payload.terminal_id)
        {
            terminal.stream = stream.clone();
            terminal.project_id = payload.start.project_id;
            terminal.root = payload.start.root;
            terminal.cwd = payload.start.cwd;
            terminal.shell = payload.start.shell;
            terminal.cols = payload.start.cols;
            terminal.rows = payload.start.rows;
            terminal.created_at_ms = payload.start.created_at_ms;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{FileEntryOp, ProjectFileEntry, ProjectFileKind, ProjectRootPath};

    fn file_entry(relative_path: &str, op: FileEntryOp) -> ProjectFileEntry {
        ProjectFileEntry {
            relative_path: relative_path.to_owned(),
            kind: ProjectFileKind::File,
            op,
        }
    }

    fn root_listing(root: &str, entries: Vec<ProjectFileEntry>) -> protocol::ProjectRootListing {
        protocol::ProjectRootListing {
            root: ProjectRootPath(root.to_owned()),
            entries,
        }
    }

    fn welcome_envelope(stream: &str, seq: u64) -> Envelope {
        Envelope::from_payload(
            StreamPath(stream.to_owned()),
            FrameKind::Welcome,
            seq,
            &protocol::WelcomePayload {
                protocol_version: protocol::PROTOCOL_VERSION,
                tyde_version: protocol::TYDE_VERSION,
                release_version: None,
            },
        )
        .expect("synthetic Welcome")
    }

    fn empty_host_bootstrap_envelope(stream: &str, seq: u64) -> Envelope {
        Envelope::from_payload(
            StreamPath(stream.to_owned()),
            FrameKind::HostBootstrap,
            seq,
            &HostBootstrapPayload {
                settings: protocol::HostSettings {
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
                mobile_access: MobileAccessStatePayload {
                    broker_status: protocol::MobileBrokerStatus::Disabled,
                    pairing: MobilePairingState::Idle,
                    paired_devices: Vec::new(),
                },
                backend_setup: BackendSetupPayload {
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
                agents: Vec::new(),
                task_token_usages: Vec::new(),
                workflow_summaries: Vec::new(),
                workflow_diagnostics: Vec::new(),
                workflow_runs: Vec::new(),
                workflow_locations: Vec::new(),
                agents_view_preferences: None,
            },
        )
        .expect("synthetic HostBootstrap")
    }

    fn project_bootstrap_envelope(project_id: &ProjectId, name: &str, seq: u64) -> Envelope {
        Envelope::from_payload(
            StreamPath(format!("/project/{}", project_id.0)),
            FrameKind::ProjectBootstrap,
            seq,
            &ProjectBootstrapPayload {
                project: protocol::Project {
                    id: project_id.clone(),
                    name: name.to_owned(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone {
                        roots: vec![protocol::ProjectRootPath("/tmp/tyde-project".to_owned())],
                    },
                },
                file_list: ProjectFileListPayload {
                    incremental: false,
                    roots: Vec::new(),
                },
                git_status: ProjectGitStatusPayload { roots: Vec::new() },
                review_summaries: Vec::new(),
            },
        )
        .expect("synthetic ProjectBootstrap")
    }

    fn project_name(state: &AppState, host_id: &str, project_id: &ProjectId) -> Option<String> {
        state.projects.with_untracked(|projects| {
            projects
                .iter()
                .find(|entry| entry.host_id == host_id && entry.project.id == *project_id)
                .map(|entry| entry.project.name.clone())
        })
    }

    #[test]
    fn auto_force_upgrade_allows_managed_incompatible_once() {
        assert!(should_auto_force_upgrade(
            RejectCode::IncompatibleProtocol,
            true,
            false,
        ));
    }

    #[test]
    fn auto_force_upgrade_rejects_already_attempted() {
        assert!(!should_auto_force_upgrade(
            RejectCode::IncompatibleProtocol,
            true,
            true,
        ));
    }

    #[test]
    fn auto_force_upgrade_rejects_non_managed_hosts() {
        assert!(!should_auto_force_upgrade(
            RejectCode::IncompatibleProtocol,
            false,
            false,
        ));
    }

    #[test]
    fn auto_force_upgrade_rejects_invalid_handshake() {
        assert!(!should_auto_force_upgrade(
            RejectCode::InvalidHandshake,
            true,
            false,
        ));
    }

    #[test]
    fn replayed_project_bootstrap_after_host_reconnect_is_accepted() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_id = "dispatch-reconnect-host";
            let project_id = ProjectId("dispatch-reconnect-project".to_owned());

            reset_inbound_state_for_host(host_id);

            dispatch_envelope(&state, host_id, welcome_envelope("/host/reconnect-a", 0));
            dispatch_envelope(
                &state,
                host_id,
                empty_host_bootstrap_envelope("/host/reconnect-a", 1),
            );
            dispatch_envelope(
                &state,
                host_id,
                project_bootstrap_envelope(&project_id, "First connection", 0),
            );
            assert_eq!(
                project_name(&state, host_id, &project_id).as_deref(),
                Some("First connection")
            );

            state.clear_host_runtime(host_id);
            assert_eq!(project_name(&state, host_id, &project_id), None);

            dispatch_envelope(&state, host_id, welcome_envelope("/host/reconnect-b", 0));
            dispatch_envelope(
                &state,
                host_id,
                empty_host_bootstrap_envelope("/host/reconnect-b", 1),
            );
            dispatch_envelope(
                &state,
                host_id,
                project_bootstrap_envelope(&project_id, "Second connection", 0),
            );

            assert_eq!(
                project_name(&state, host_id, &project_id).as_deref(),
                Some("Second connection"),
                "replayed ProjectBootstrap seq 0 must be validated and applied after reconnect"
            );
        });
    }

    #[test]
    fn file_list_preserves_same_relative_path_in_different_roots() {
        let project_id = ProjectId("project".to_owned());
        let mut file_tree = HashMap::new();

        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: false,
                roots: vec![
                    root_listing(
                        "/repo/root-a",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                    root_listing(
                        "/repo/root-b",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                ],
            },
        );

        let roots = file_tree.get(&project_id).expect("project file tree");
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].root.0, "/repo/root-a");
        assert_eq!(roots[1].root.0, "/repo/root-b");
        assert_eq!(roots[0].entries[0].relative_path, "same.txt");
        assert_eq!(roots[1].entries[0].relative_path, "same.txt");
    }

    #[test]
    fn stream_end_then_metadata_updated_patches_existing_row() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent_id = AgentId("a-stream-meta".to_owned());
            let message_id = protocol::ChatMessageId("msg-stream-meta".to_owned());

            apply_chat_event(
                &state,
                "host-1",
                &agent_id,
                ChatEvent::StreamStart(protocol::StreamStartData {
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
                "host-1",
                &agent_id,
                ChatEvent::StreamEnd(protocol::StreamEndData {
                    message: chat_message,
                }),
            );

            let row_id_before = state
                .chat_rows
                .with_untracked(|m| m.get(&agent_id).map(|rows| rows[0].id))
                .expect("row created by StreamEnd");

            apply_chat_event(
                &state,
                "host-1",
                &agent_id,
                ChatEvent::MessageMetadataUpdated(protocol::MessageMetadataUpdateData {
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
                .chat_rows
                .with_untracked(|m| m.get(&agent_id).cloned())
                .expect("agent rows");
            assert_eq!(
                rows.len(),
                1,
                "MessageMetadataUpdated must not append a new row after StreamEnd"
            );
            assert_eq!(
                rows[0].id, row_id_before,
                "row identity preserved by in-place patch"
            );
            let entry = rows[0].entry.get_untracked();
            assert_eq!(entry.message.content, "streamed body");
            assert!(
                entry
                    .message
                    .model_info
                    .as_ref()
                    .is_some_and(|m| m.model == "gpt-test"),
                "model_info patched"
            );
            assert!(
                entry
                    .message
                    .token_usage
                    .as_ref()
                    .and_then(|t| t.request.known_usage())
                    .is_some_and(|u| u.total_tokens == 10),
                "token_usage request scope patched"
            );
        });
    }

    #[test]
    fn agent_bootstrap_mid_turn_keeps_live_stream_end_visible() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_id = "midturn-host";
            let agent_id = AgentId("a-midturn".to_owned());
            let stream = StreamPath("/agent/a-midturn/inst".to_owned());

            state.agents.update(|agents| {
                agents.push(AgentInfo {
                    host_id: host_id.to_owned(),
                    agent_id: agent_id.clone(),
                    name: "Midturn Agent".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    project_id: None,
                    parent_agent_id: None,
                    session_id: None,
                    custom_agent_id: None,
                    workflow: None,
                    created_at_ms: 0,
                    instance_stream: stream.clone(),
                    started: true,
                    fatal_error: None,
                    activity_summary: Default::default(),
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
            let mut events = vec![AgentBootstrapEvent::HasPriorHistory {
                message_count: 20,
                before_seq: 42,
            }];
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
                host_id,
                &stream,
                AgentBootstrapPayload {
                    events,
                    latest_output: Default::default(),
                },
            );

            assert!(
                state
                    .session_history
                    .with_untracked(|map| map
                        .get(&agent_id)
                        .is_some_and(
                            |history| history.message_count == 20 && history.oldest_seq == Some(42)
                        )),
                "bootstrap should retain the server-provided prior-history indicator"
            );
            assert_eq!(
                state
                    .streaming_text
                    .with_untracked(|map| map.get(&agent_id).map(|s| s.text.get_untracked())),
                Some("partial".to_owned()),
                "active stream delta should be restored"
            );

            apply_chat_event(
                &state,
                host_id,
                &agent_id,
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
                    .chat_rows
                    .with_untracked(|map| map.get(&agent_id).map(Vec::len)),
                Some(1),
                "live StreamEnd should append while prior history remains unloaded"
            );
        });
    }

    #[test]
    fn session_history_prepends_newest_first_page() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_id = "history-host";
            let agent_id = AgentId("a-history".to_owned());
            let stream = StreamPath("/agent/a-history/inst".to_owned());

            state.agents.update(|agents| {
                agents.push(AgentInfo {
                    host_id: host_id.to_owned(),
                    agent_id: agent_id.clone(),
                    name: "History Agent".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Codex,
                    workspace_roots: Vec::new(),
                    project_id: None,
                    parent_agent_id: None,
                    session_id: None,
                    custom_agent_id: None,
                    workflow: None,
                    created_at_ms: 0,
                    instance_stream: stream.clone(),
                    started: true,
                    fatal_error: None,
                    activity_summary: Default::default(),
                });
            });

            let message = |content: &str, sender: protocol::MessageSender| protocol::ChatMessage {
                message_id: None,
                timestamp: 0,
                sender,
                content: content.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            };

            state.push_chat_entry(
                agent_id.clone(),
                ChatMessageEntry {
                    message: message(
                        "live row",
                        protocol::MessageSender::Assistant {
                            agent: "History Agent".to_owned(),
                        },
                    ),
                    tool_requests: Vec::new(),
                },
            );
            state.session_history.update(|map| {
                map.insert(
                    agent_id.clone(),
                    SessionHistoryState {
                        message_count: 2,
                        oldest_seq: Some(12),
                        has_more_before: true,
                        loading: true,
                    },
                );
            });

            apply_session_history(
                &state,
                host_id,
                &stream,
                SessionHistoryPayload {
                    agent_id: agent_id.clone(),
                    events: vec![
                        ChatEvent::MessageAdded(message(
                            "older row 2",
                            protocol::MessageSender::User,
                        )),
                        ChatEvent::MessageAdded(message(
                            "older row 1",
                            protocol::MessageSender::User,
                        )),
                    ],
                    has_more_before: false,
                    oldest_seq: Some(3),
                },
            );

            let rows = state
                .chat_rows
                .with_untracked(|map| map.get(&agent_id).cloned())
                .expect("history rows");
            let contents = rows
                .iter()
                .map(|row| row.entry.get_untracked().message.content)
                .collect::<Vec<_>>();
            assert_eq!(
                contents,
                vec![
                    "older row 1".to_owned(),
                    "older row 2".to_owned(),
                    "live row".to_owned()
                ]
            );
            assert!(
                state
                    .session_history
                    .with_untracked(|map| !map.contains_key(&agent_id)),
                "final page should remove the load-older affordance"
            );
        });
    }

    #[test]
    fn code_intel_error_frame_records_file_error() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_id = "code-intel-error-host";
            let project_id = ProjectId("code-intel-error-project".to_owned());
            let path = ProjectPath {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/main.rs".to_owned(),
            };

            prime_host_for_tests(&state, host_id);
            dispatch_envelope(
                &state,
                host_id,
                project_bootstrap_envelope(&project_id, "Project", 0),
            );
            state.open_files.update(|files| {
                files.insert(
                    path.clone(),
                    OpenFile {
                        path: path.clone(),
                        version: protocol::ProjectFileVersion(1),
                        contents: Some("fn main() {}".to_owned()),
                        is_binary: false,
                    },
                );
            });
            let key = CodeIntelKey {
                host_id: host_id.to_owned(),
                project_id: project_id.clone(),
                path: path.clone(),
            };
            state.code_intel.update(|map| {
                map.entry(key.clone())
                    .or_default()
                    .set_rendered_version(protocol::ProjectFileVersion(1));
            });

            let payload = protocol::CodeIntelErrorPayload {
                code: protocol::CodeIntelErrorCode::Internal,
                message: "semanticTokens/full failed".to_owned(),
                hint: None,
                exit_status: None,
                stderr: None,
                context: protocol::CodeIntelErrorContext::Subscribe { path },
                fatal: false,
            };
            let envelope = Envelope::from_payload(
                StreamPath(format!("/project/{}", project_id.0)),
                FrameKind::CodeIntelError,
                1,
                &payload,
            )
            .expect("synthetic CodeIntelError");
            dispatch_envelope(&state, host_id, envelope);

            state.code_intel.with_untracked(|map| {
                let error = map
                    .get(&key)
                    .and_then(|file| file.applied())
                    .and_then(|data| data.error.as_ref())
                    .expect("error recorded for rendered file");
                assert_eq!(error.message, "semanticTokens/full failed");
            });
        });
    }

    #[test]
    fn file_list_remove_is_scoped_to_root() {
        let project_id = ProjectId("project".to_owned());
        let mut file_tree = HashMap::new();

        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: false,
                roots: vec![
                    root_listing(
                        "/repo/root-a",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                    root_listing(
                        "/repo/root-b",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                ],
            },
        );
        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: true,
                roots: vec![root_listing(
                    "/repo/root-a",
                    vec![file_entry("same.txt", FileEntryOp::Remove)],
                )],
            },
        );

        let roots = file_tree.get(&project_id).expect("project file tree");
        let root_a = roots
            .iter()
            .find(|root| root.root.0 == "/repo/root-a")
            .expect("root-a");
        let root_b = roots
            .iter()
            .find(|root| root.root.0 == "/repo/root-b")
            .expect("root-b");
        assert!(root_a.entries.is_empty());
        assert_eq!(root_b.entries[0].relative_path, "same.txt");
    }
}

/// §M4 frontend external-change correctness, exercised end-to-end through the
/// real dispatch + send path in a headless browser. These need the wasm
/// transport (the refresh subscribe is a network send), so they are wasm-only.
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, CodeIntelKey};
    use protocol::{CodeIntelDiagnostic, ProjectFileVersion, ProjectPath, ProjectRootPath};
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Yield to the browser event loop so `spawn_local` sends flush.
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Install a Tauri `invoke` stub that records every outbound call into
    /// `window.__test_send_calls`, so a test can inspect the frames put on the
    /// wire.
    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");
    }

    /// Count `code_intel_subscribe_file` frames captured by the send stub.
    fn subscribe_frames_sent() -> u32 {
        js_sys::eval(
            r#"
            (function() {
                let n = 0;
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "code_intel_subscribe_file") n++;
                }
                return n;
            })()
            "#,
        )
        .expect("probe send calls")
        .as_f64()
        .unwrap_or(0.0) as u32
    }

    fn project_bootstrap_envelope(project_id: &ProjectId, seq: u64) -> Envelope {
        Envelope::from_payload(
            StreamPath(format!("/project/{}", project_id.0)),
            FrameKind::ProjectBootstrap,
            seq,
            &ProjectBootstrapPayload {
                project: protocol::Project {
                    id: project_id.clone(),
                    name: "M4".to_owned(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/repo".to_owned())],
                    },
                },
                file_list: ProjectFileListPayload {
                    incremental: false,
                    roots: Vec::new(),
                },
                git_status: ProjectGitStatusPayload { roots: Vec::new() },
                review_summaries: Vec::new(),
            },
        )
        .expect("synthetic ProjectBootstrap")
    }

    fn file_contents_envelope(
        project_id: &ProjectId,
        path: &ProjectPath,
        version: ProjectFileVersion,
        seq: u64,
    ) -> Envelope {
        Envelope::from_payload(
            StreamPath(format!("/project/{}", project_id.0)),
            FrameKind::ProjectFileContents,
            seq,
            &ProjectFileContentsPayload {
                path: path.clone(),
                version,
                contents: Some("fn main() {}".to_owned()),
                is_binary: false,
            },
        )
        .expect("synthetic ProjectFileContents")
    }

    fn stale_diagnostic(message: &str) -> CodeIntelDiagnostic {
        CodeIntelDiagnostic {
            range: protocol::ByteRange { start: 0, end: 1 },
            severity: protocol::CodeIntelSeverity::Error,
            message: message.to_owned(),
            source: None,
        }
    }

    #[wasm_bindgen_test]
    async fn reload_at_new_version_drops_stale_and_sends_refresh_subscribe() {
        install_send_stub();
        let state = AppState::new();
        let host_id = "m4-host";
        let project_id = ProjectId("m4-project".to_owned());
        let path = ProjectPath {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/main.rs".to_owned(),
        };

        prime_host_for_tests(&state, host_id);
        dispatch_envelope(&state, host_id, project_bootstrap_envelope(&project_id, 0));

        // First contents at v1: this is the initial open, not a reload (no
        // prior rendered version), so no refresh subscribe fires.
        dispatch_envelope(
            &state,
            host_id,
            file_contents_envelope(&project_id, &path, ProjectFileVersion(1), 1),
        );
        for _ in 0..5 {
            next_tick().await;
        }
        assert_eq!(
            subscribe_frames_sent(),
            0,
            "initial contents must not trigger a refresh subscribe"
        );

        // Stash a v1 decoration so we can prove it is dropped on the reload.
        let key = CodeIntelKey {
            host_id: host_id.to_owned(),
            project_id: project_id.clone(),
            path: path.clone(),
        };
        state.code_intel.update(|map| {
            if let Some(entry) = map.get_mut(&key) {
                entry.merge_versioned(ProjectFileVersion(1), |d| {
                    d.diagnostics = vec![stale_diagnostic("stale")];
                });
            }
        });

        // Reload: v2 contents arrive (external edit / branch switch).
        dispatch_envelope(
            &state,
            host_id,
            file_contents_envelope(&project_id, &path, ProjectFileVersion(2), 2),
        );
        for _ in 0..5 {
            next_tick().await;
        }

        // The rendered version advanced and the stale v1 decorations were
        // dropped (never paint v1 over v2 text).
        state.code_intel.with_untracked(|map| {
            let entry = map.get(&key).expect("code-intel entry");
            assert_eq!(entry.rendered_version, Some(ProjectFileVersion(2)));
            assert!(
                !entry.by_version.contains_key(&ProjectFileVersion(1)),
                "stale v1 decorations must be dropped on reload"
            );
        });

        // A fresh refresh subscribe was sent so the server re-pushes at the new
        // version.
        assert_eq!(
            subscribe_frames_sent(),
            1,
            "a reload to a newer version must send exactly one refresh subscribe"
        );

        // A late v1 frame arriving after the bump is ignored (dropped), not
        // stashed — it can never paint over v2.
        state.code_intel.update(|map| {
            if let Some(entry) = map.get_mut(&key) {
                entry.merge_versioned(ProjectFileVersion(1), |d| {
                    d.diagnostics = vec![stale_diagnostic("late")];
                });
            }
        });
        state.code_intel.with_untracked(|map| {
            let entry = map.get(&key).expect("code-intel entry");
            assert!(
                !entry.by_version.contains_key(&ProjectFileVersion(1)),
                "a late old-version frame after the bump must be dropped"
            );
        });
    }

    // ── M5: find-references dispatch correlation ────────────────────────────

    fn references_results_envelope(
        project_id: &ProjectId,
        references_id: u64,
        relative: &str,
        seq: u64,
    ) -> Envelope {
        Envelope::from_payload(
            StreamPath(format!("/project/{}", project_id.0)),
            FrameKind::CodeIntelReferencesResults,
            seq,
            &protocol::CodeIntelReferencesResultsPayload {
                references_id,
                file: protocol::CodeIntelReferencesFileResult {
                    path: ProjectPath {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: relative.to_owned(),
                    },
                    lines: vec![protocol::CodeIntelReferenceLine {
                        line_number: 1,
                        line_text: "foo();".to_owned(),
                        ranges: vec![protocol::ByteRange { start: 0, end: 3 }],
                    }],
                    truncated: false,
                },
            },
        )
        .expect("synthetic CodeIntelReferencesResults")
    }

    fn references_complete_envelope(
        project_id: &ProjectId,
        references_id: u64,
        total_files: u32,
        total_references: u32,
        seq: u64,
    ) -> Envelope {
        Envelope::from_payload(
            StreamPath(format!("/project/{}", project_id.0)),
            FrameKind::CodeIntelReferencesComplete,
            seq,
            &protocol::CodeIntelReferencesCompletePayload {
                references_id,
                total_files,
                total_references,
                truncated: false,
                cancelled: false,
                error: None,
            },
        )
        .expect("synthetic CodeIntelReferencesComplete")
    }

    /// The most recent `code_intel_find_references` frame the send stub captured,
    /// as `(references_id, offset)`. `None` if no such frame was sent.
    fn last_find_references_frame() -> Option<(u64, u32)> {
        let value = js_sys::eval(
            r#"
            (function() {
                for (let i = (window.__test_send_calls || []).length - 1; i >= 0; i--) {
                    const [cmd, args] = window.__test_send_calls[i];
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "code_intel_find_references") {
                        return env.payload.references_id + ":" + env.payload.offset;
                    }
                }
                return null;
            })()
            "#,
        )
        .expect("probe find-references frame");
        let text = value.as_string()?;
        let (id, offset) = text.split_once(':')?;
        Some((id.parse().ok()?, offset.parse().ok()?))
    }

    /// `start_find_references` (the Shift+F12 entry point) mints a references id,
    /// sends a `code_intel_find_references` carrying the requested byte offset,
    /// switches the left dock to the References tab, and primes the panel as
    /// in-flight for that id.
    #[wasm_bindgen_test]
    async fn start_find_references_sends_frame_and_opens_panel() {
        install_send_stub();
        let state = AppState::new();
        let host_id = "m5-send-host";
        let project_id = ProjectId("m5-send-project".to_owned());
        prime_host_for_tests(&state, host_id);
        dispatch_envelope(&state, host_id, project_bootstrap_envelope(&project_id, 0));
        state
            .active_project
            .set(Some(crate::state::ActiveProjectRef {
                host_id: host_id.to_owned(),
                project_id: project_id.clone(),
            }));

        let path = ProjectPath {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/main.rs".to_owned(),
        };
        crate::actions::start_find_references(
            &state,
            path,
            ProjectFileVersion(1),
            12,
            Some("foo".to_owned()),
        );
        for _ in 0..5 {
            next_tick().await;
        }

        // The panel is primed in-flight for the freshly-minted id, on the Refs tab.
        let active_id = state
            .references_state
            .with_untracked(|s| s.active_references_id);
        assert!(active_id > 0, "a references id was minted");
        assert!(state.references_state.with_untracked(|s| s.in_flight));
        assert_eq!(
            state.references_state.with_untracked(|s| s.symbol.clone()),
            Some("foo".to_owned())
        );
        assert_eq!(
            state.left_tab.get_untracked(),
            crate::state::LeftTab::References
        );

        // The frame went out with the requested offset and the minted id.
        let frame = last_find_references_frame().expect("a find-references frame was sent");
        assert_eq!(
            frame,
            (active_id, 12),
            "frame carries the minted id + offset"
        );
    }

    /// Streamed references frames populate the panel only for the active
    /// `references_id`; a superseded (older id) frame is ignored, and the
    /// terminal complete records the totals + clears the in-flight flag.
    #[wasm_bindgen_test]
    async fn references_apply_only_for_active_id() {
        let state = AppState::new();
        let host_id = "m5-host";
        let project_id = ProjectId("m5-project".to_owned());
        prime_host_for_tests(&state, host_id);
        dispatch_envelope(&state, host_id, project_bootstrap_envelope(&project_id, 0));

        state
            .active_project
            .set(Some(crate::state::ActiveProjectRef {
                host_id: host_id.to_owned(),
                project_id: project_id.clone(),
            }));

        // The newest query has id 2 active and is in flight for this project.
        state.references_state.update(|s| {
            s.host_id = Some(host_id.to_owned());
            s.project_id = Some(project_id.clone());
            s.source_path = Some(ProjectPath {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/main.rs".to_owned(),
            });
            s.source_version = Some(ProjectFileVersion(1));
            s.active_references_id = 2;
            s.in_flight = true;
        });

        // A superseded (older id 1) results frame is dropped.
        dispatch_envelope(
            &state,
            host_id,
            references_results_envelope(&project_id, 1, "src/old.rs", 1),
        );
        assert!(
            state
                .references_state
                .with_untracked(|s| s.results.is_empty()),
            "a superseded older references-id result must be ignored"
        );

        // A results frame for the active id populates the panel.
        dispatch_envelope(
            &state,
            host_id,
            references_results_envelope(&project_id, 2, "src/a.rs", 2),
        );
        state.references_state.with_untracked(|s| {
            assert_eq!(s.results.len(), 1, "active-id result populates the panel");
            assert_eq!(s.results[0].path.relative_path, "src/a.rs");
            assert_eq!(s.total_files, 1, "running file total updates per result");
            assert_eq!(
                s.total_references, 1,
                "running reference total updates per result"
            );
            assert!(s.in_flight, "still in flight until the terminal frame");
        });

        // The terminal complete for the active id records totals + finishes.
        dispatch_envelope(
            &state,
            host_id,
            references_complete_envelope(&project_id, 2, 3, 7, 3),
        );
        state.references_state.with_untracked(|s| {
            assert!(!s.in_flight, "complete clears the in-flight flag");
            assert_eq!(s.total_files, 3);
            assert_eq!(s.total_references, 7);
        });
    }

    #[wasm_bindgen_test]
    async fn project_switch_clears_references_and_drops_late_project_frames() {
        let state = AppState::new();
        let host_id = "m5-switch-host";
        let project_a = ProjectId("project-a".to_owned());
        let project_b = ProjectId("project-b".to_owned());
        prime_host_for_tests(&state, host_id);
        dispatch_envelope(&state, host_id, project_bootstrap_envelope(&project_a, 0));
        dispatch_envelope(&state, host_id, project_bootstrap_envelope(&project_b, 0));

        let source = ProjectPath {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/main.rs".to_owned(),
        };
        state
            .active_project
            .set(Some(crate::state::ActiveProjectRef {
                host_id: host_id.to_owned(),
                project_id: project_a.clone(),
            }));
        state.references_state.set(ProjectReferencesUiState {
            host_id: Some(host_id.to_owned()),
            project_id: Some(project_a.clone()),
            source_path: Some(source),
            source_version: Some(ProjectFileVersion(1)),
            active_references_id: 9,
            in_flight: true,
            ..Default::default()
        });

        state.switch_active_project(Some(crate::state::ActiveProjectRef {
            host_id: host_id.to_owned(),
            project_id: project_b.clone(),
        }));
        state.references_state.with_untracked(|s| {
            assert_eq!(
                s.active_references_id, 0,
                "switching projects clears/cancels the references panel state"
            );
            assert!(s.results.is_empty());
        });

        dispatch_envelope(
            &state,
            host_id,
            references_results_envelope(&project_a, 9, "src/a.rs", 1),
        );
        state.references_state.with_untracked(|s| {
            assert!(
                s.results.is_empty(),
                "late references from the previously-active project must be ignored"
            );
            assert_eq!(s.total_references, 0);
        });
    }
}
