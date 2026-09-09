//! The scriptable mock backend used by server protocol tests.

mod actor;
mod control;
mod emit;
mod gate;
mod script;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentInput, BackendAccessMode, BackendKind, ChatMessageId, CompactionMethod, CompactionMetrics,
    CompactionOperationId, CompactionStage, CompactionTrigger, SessionId, ToolPolicy,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::empty_session_settings_schema;
use super::{
    Backend, BackendAcceptedCompaction, BackendCompactionAvailability, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionCoordinator,
    BackendCompactionDeferredReason, BackendCompactionDispatchState, BackendCompactionEvent,
    BackendCompactionMechanism, BackendCompactionMutationState,
    BackendCompactionNotDispatchedReason, BackendCompactionProgress, BackendCompactionRequest,
    BackendCompactionResult, BackendCompactionStart, BackendCompactionSuccess,
    BackendCompactionTerminalEvidence, BackendEvent, BackendSession, BackendSpawnConfig,
    BackendStartupError, EventStream, PostCompactionTokenCount, StartupMcpTransport,
};

use actor::{MockLoopConfig, start_mock_command_loop};
use control::MockCommand;
use emit::{MockEventSender, WeakMockEventSender};

pub use control::{MockControl, MockRequest, MockViolation};
pub use gate::MockGateHandle;
pub use script::{MockCompactionFailure, MockLaunch, MockScript, MockTurn};

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct MockDiscovery {
    responses: Arc<Mutex<std::collections::VecDeque<Result<super::BackendDiscovery, String>>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl MockDiscovery {
    pub fn new(responses: Vec<Result<super::BackendDiscovery, String>>) -> Self {
        assert!(
            !responses.is_empty(),
            "mock discovery needs at least one response"
        );
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
        }
    }

    fn next(&self) -> Result<super::BackendDiscovery, String> {
        let mut responses = self
            .responses
            .lock()
            .expect("mock discovery response mutex");
        if responses.len() > 1 {
            responses.pop_front().expect("response exists")
        } else {
            responses.front().expect("response exists").clone()
        }
    }
}

const MOCK_MODEL: &str = "mock";

#[derive(Debug, Clone)]
struct MockSessionRecord {
    workspace_roots: Vec<String>,
    prompts: Vec<String>,
    /// Sticky across resume and fork.
    user_bubbles: bool,
    startup_mcp_servers: Vec<String>,
    instructions: Option<String>,
    steering_body: String,
    skills: Vec<String>,
    tool_policy: ToolPolicy,
    access_mode: BackendAccessMode,
    compaction_capability: BackendCompactionCapability,
    created_at_ms: u64,
    updated_at_ms: u64,
}

fn session_store() -> &'static Mutex<HashMap<String, MockSessionRecord>> {
    static STORE: OnceLock<Mutex<HashMap<String, MockSessionRecord>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct MockBackend {
    command_tx: mpsc::UnboundedSender<MockCommand>,
    events_tx: Option<WeakMockEventSender>,
    session_id: SessionId,
    busy_self_turn_fired: Arc<std::sync::atomic::AtomicBool>,
    active_compaction: Arc<Mutex<Option<MockCompactionFlight>>>,
    compaction_capability: BackendCompactionCapability,
    /// Handle to the actor's test-control surface, served through
    /// `Backend::mock_control` / `AgentCommand::ReadMockControl`.
    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    control: MockControl,
    scripted_busy_self_turn: bool,
    shutdown_gate: Option<gate::MockGate>,
    compaction_observation_gates: Option<(gate::MockGate, gate::MockGate)>,
    compaction_failure: Option<MockCompactionFailure>,
    resume_replay_guard: Option<tokio::sync::oneshot::Sender<()>>,
}

struct MockCompactionFlight {
    operation_id: CompactionOperationId,
    terminal_tx: Option<tokio::sync::oneshot::Sender<BackendCompactionResult>>,
}

impl MockBackend {
    /// The launch script is the host's mock-launch reservation, carried in
    /// `config.mock_launch`. It is installed in the actor's configuration
    /// before the command loop starts, so it governs the launch turn with no
    /// binding race by construction.
    async fn spawn_with_launch(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let launch_script = match config.mock_launch.clone() {
            None => default_mock_script(),
            Some(MockLaunch::Script(script)) => script,
            Some(MockLaunch::CloseBeforeResumeBarrier) => {
                return Err(
                    "mock close-before-resume-barrier launch behavior applies only to resume"
                        .to_owned(),
                );
            }
        };
        let scripted_busy_self_turn = launch_script.busy_self_turn_once;
        let shutdown_gate = launch_script.shutdown_gate.clone();
        let compaction_observation_gates = launch_script.compaction_observation_gates.clone();
        let compaction_failure = launch_script.compaction_failure;
        let initial_message = initial_input.message;
        let agent_control_await_mcp = emit::agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = summarize_startup_mcp_servers(&config);
        let session_id = SessionId(Uuid::new_v4().to_string());
        let now = now_ms();
        let resolved_spawn_config = config.resolved_spawn_config.clone();
        let mut compaction_capability = native_mock_compaction_capability();
        if let Some(availability) = launch_script.compaction_availability.clone() {
            compaction_capability.availability = availability;
        }

        {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            store.insert(
                session_id.0.clone(),
                MockSessionRecord {
                    workspace_roots,
                    prompts: Vec::new(),
                    user_bubbles: launch_script.user_bubbles,
                    startup_mcp_servers: startup_mcp_servers.clone(),
                    instructions: resolved_spawn_config.instructions,
                    steering_body: resolved_spawn_config.steering_body,
                    skills: resolved_spawn_config
                        .skills
                        .into_iter()
                        .map(|skill| summarize_skill(&skill))
                        .collect(),
                    tool_policy: resolved_spawn_config.tool_policy,
                    access_mode: resolved_spawn_config.access_mode,
                    compaction_capability: compaction_capability.clone(),
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
        }

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (backend_events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let events_tx = MockEventSender::new(backend_events_tx);
        let subagent_emitter = config.subagent_emitter.clone();
        let (control, control_rx, terminal_report) = MockControl::channel();
        let session_id_for_task = session_id.clone();

        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx.clone(),
            subagent_emitter,
            control_rx,
            terminal_report,
            MockLoopConfig {
                initial_message: Some(initial_message),
                user_bubbles_from_history: false,
                agent_control_await_mcp,
                launch_script,
            },
        );

        Ok((
            Self {
                command_tx,
                events_tx: Some(events_tx.downgrade()),
                session_id,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                active_compaction: Arc::new(Mutex::new(None)),
                compaction_capability,
                control,
                scripted_busy_self_turn,
                shutdown_gate: shutdown_gate.clone(),
                compaction_observation_gates: compaction_observation_gates.clone(),
                compaction_failure,
                resume_replay_guard: None,
            },
            EventStream::new_backend(events_rx),
        ))
    }

    /// [`Backend::resume`] plus an optional launch script (see
    /// [`MockBackend::spawn_with_launch`]). A launch reservation applies to
    /// every newly created backend instance, including resume and fork.
    async fn resume_with_launch(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (launch_script, close_before_barrier) = match config.mock_launch.clone() {
            None => (default_mock_script(), false),
            Some(MockLaunch::Script(script)) => (script, false),
            Some(MockLaunch::CloseBeforeResumeBarrier) => (default_mock_script(), true),
        };
        let scripted_busy_self_turn = launch_script.busy_self_turn_once;
        let shutdown_gate = launch_script.shutdown_gate.clone();
        let compaction_observation_gates = launch_script.compaction_observation_gates.clone();
        let compaction_failure = launch_script.compaction_failure;
        let agent_control_await_mcp = emit::agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = summarize_startup_mcp_servers(&config);
        let resolved_spawn_config = config.resolved_spawn_config.clone();
        let (replay_prompts, session_user_bubbles, compaction_capability) = {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            let Some(record) = store.get_mut(&session_id.0) else {
                return Err(format!("unknown mock session {}", session_id.0));
            };
            let replay_prompts = record.prompts.clone();
            record.user_bubbles |= launch_script.user_bubbles;
            let user_bubbles = record.user_bubbles;
            record.workspace_roots = workspace_roots;
            record.startup_mcp_servers = startup_mcp_servers;
            record.instructions = resolved_spawn_config.instructions;
            record.steering_body = resolved_spawn_config.steering_body;
            record.skills = resolved_spawn_config
                .skills
                .into_iter()
                .map(|skill| summarize_skill(&skill))
                .collect();
            record.tool_policy = resolved_spawn_config.tool_policy;
            record.access_mode = resolved_spawn_config.access_mode;
            record.updated_at_ms = now_ms();
            (
                replay_prompts,
                user_bubbles,
                record.compaction_capability.clone(),
            )
        };

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (backend_events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let events_tx = MockEventSender::new(backend_events_tx);
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let subagent_emitter = config.subagent_emitter.clone();
        let (control, control_rx, terminal_report) = MockControl::channel();
        let session_id_for_task = session_id.clone();

        if close_before_barrier {
            return Ok((
                Self {
                    command_tx,
                    events_tx: None,
                    session_id,
                    busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    active_compaction: Arc::new(Mutex::new(None)),
                    compaction_capability,
                    control,
                    scripted_busy_self_turn,
                    shutdown_gate: shutdown_gate.clone(),
                    compaction_observation_gates: compaction_observation_gates.clone(),
                    compaction_failure,
                    resume_replay_guard: Some(resume_replay_complete_tx),
                },
                EventStream::new_backend_with_resume_replay_barrier(
                    events_rx,
                    resume_replay_complete_rx,
                ),
            ));
        }

        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx.clone(),
            subagent_emitter,
            control_rx,
            terminal_report,
            MockLoopConfig {
                initial_message: None,
                user_bubbles_from_history: session_user_bubbles,
                agent_control_await_mcp,
                launch_script,
            },
        );

        emit_resume_history(
            &events_tx,
            &session_id,
            &replay_prompts,
            session_user_bubbles,
        );
        let _ = resume_replay_complete_tx.send(());

        Ok((
            Self {
                command_tx,
                events_tx: Some(events_tx.downgrade()),
                session_id,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                active_compaction: Arc::new(Mutex::new(None)),
                compaction_capability,
                control,
                scripted_busy_self_turn,
                shutdown_gate: shutdown_gate.clone(),
                compaction_observation_gates: compaction_observation_gates.clone(),
                compaction_failure,
                resume_replay_guard: None,
            },
            EventStream::new_backend_with_resume_replay_barrier(
                events_rx,
                resume_replay_complete_rx,
            ),
        ))
    }

    /// [`Backend::fork`] plus an optional launch script (see
    /// [`MockBackend::spawn_with_launch`]).
    async fn fork_with_launch(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        let launch_script = match config.mock_launch.clone() {
            None => default_mock_script(),
            Some(MockLaunch::Script(script)) => script,
            Some(MockLaunch::CloseBeforeResumeBarrier) => {
                return Err(BackendStartupError::backend_failed(
                    "mock close-before-resume-barrier launch behavior applies only to resume",
                ));
            }
        };
        let scripted_busy_self_turn = launch_script.busy_self_turn_once;
        let shutdown_gate = launch_script.shutdown_gate.clone();
        let compaction_observation_gates = launch_script.compaction_observation_gates.clone();
        let compaction_failure = launch_script.compaction_failure;
        let initial_message = initial_input.message;
        let agent_control_await_mcp = emit::agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = summarize_startup_mcp_servers(&config);
        let session_id = SessionId(Uuid::new_v4().to_string());
        let now = now_ms();
        let resolved_spawn_config = config.resolved_spawn_config.clone();

        let (compaction_capability, source_user_bubbles) = {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            let Some(source) = store.get(&from_session_id.0).cloned() else {
                return Err(BackendStartupError::backend_failed(format!(
                    "unknown mock session {}",
                    from_session_id.0
                )));
            };
            let compaction_capability = source.compaction_capability.clone();
            store.insert(
                session_id.0.clone(),
                MockSessionRecord {
                    workspace_roots,
                    prompts: source.prompts,
                    user_bubbles: source.user_bubbles || launch_script.user_bubbles,
                    startup_mcp_servers,
                    instructions: resolved_spawn_config.instructions,
                    steering_body: resolved_spawn_config.steering_body,
                    skills: resolved_spawn_config
                        .skills
                        .into_iter()
                        .map(|skill| summarize_skill(&skill))
                        .collect(),
                    tool_policy: resolved_spawn_config.tool_policy,
                    access_mode: resolved_spawn_config.access_mode,
                    compaction_capability: compaction_capability.clone(),
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
            (compaction_capability, source.user_bubbles)
        };

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (backend_events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let events_tx = MockEventSender::new(backend_events_tx);
        let subagent_emitter = config.subagent_emitter.clone();
        let (control, control_rx, terminal_report) = MockControl::channel();
        let session_id_for_task = session_id.clone();
        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx.clone(),
            subagent_emitter,
            control_rx,
            terminal_report,
            MockLoopConfig {
                initial_message: Some(initial_message),
                user_bubbles_from_history: source_user_bubbles,
                agent_control_await_mcp,
                launch_script,
            },
        );

        Ok((
            Self {
                command_tx,
                events_tx: Some(events_tx.downgrade()),
                session_id,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                active_compaction: Arc::new(Mutex::new(None)),
                compaction_capability,
                control,
                scripted_busy_self_turn,
                shutdown_gate: shutdown_gate.clone(),
                compaction_observation_gates: compaction_observation_gates.clone(),
                compaction_failure,
                resume_replay_guard: None,
            },
            EventStream::new_backend(events_rx),
        ))
    }
}

fn native_mock_compaction_capability() -> BackendCompactionCapability {
    BackendCompactionCapability {
        coordinator: BackendCompactionCoordinator::ContextOperation,
        availability: BackendCompactionAvailability::Native {
            mechanism: BackendCompactionMechanism::JsonRpcRequest,
        },
        provider_version: Some("mock-native-compaction-v1".to_owned()),
        protocol_version: Some("mock-native-compaction-v1".to_owned()),
        evidence: BackendCompactionCapabilityEvidence::AdapterContract,
    }
}

fn default_mock_script() -> MockScript {
    MockScript::new().with_unbounded_echo()
}

impl Backend for MockBackend {
    #[cfg(any(test, feature = "test-support"))]
    async fn discover(
        context: &super::BackendProbeContext,
    ) -> Result<super::BackendDiscovery, String> {
        context
            .mock_discovery
            .as_ref()
            .ok_or_else(|| "mock discovery not configured".to_owned())?
            .next()
    }

    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ListSessions,
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::ForkSession,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::AgentControlTools,
            tyde_agent_adapter::BackendCapability::Subagents,
            tyde_agent_adapter::BackendCapability::CompactionReported,
            tyde_agent_adapter::BackendCapability::AgentInitiatedTurns,
            tyde_agent_adapter::BackendCapability::GenericOtherTool,
        ]
        .into()
    }

    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Claude)
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_launch(workspace_roots, config, initial_input).await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        Self::resume_with_launch(workspace_roots, config, session_id).await
    }

    async fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Self::fork_with_launch(workspace_roots, config, from_session_id, initial_input).await
    }

    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<MockControl> {
        Some(self.control.clone())
    }

    async fn list_sessions(
        _context: &crate::backend::BackendProbeContext,
    ) -> Result<Vec<BackendSession>, String> {
        let store = session_store()
            .lock()
            .expect("mock backend session store mutex poisoned");
        let mut sessions: Vec<_> = store
            .iter()
            .map(|(id, record)| BackendSession {
                id: SessionId(id.clone()),
                backend_kind: BackendKind::Claude,
                workspace_roots: record.workspace_roots.clone(),
                title: Some(format!("Mock session {}", &id[..8.min(id.len())])),
                token_count: None,
                created_at_ms: Some(record.created_at_ms),
                updated_at_ms: Some(record.updated_at_ms),
                resumable: true,
            })
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(sessions)
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.compaction_capability.clone()
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        if let Some(start) =
            super::compaction::not_dispatched_for_capability(&self.compaction_capability)
        {
            return start;
        }
        let Some(events_tx) = self
            .events_tx
            .as_ref()
            .and_then(WeakMockEventSender::upgrade)
        else {
            return BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::BackendClosed,
            };
        };
        if events_tx.is_active() {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::ActiveTurn,
            };
        }
        if self
            .active_compaction
            .lock()
            .expect("mock active compaction mutex poisoned")
            .is_some()
        {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::AnotherCompactionActive,
            };
        }
        if let Some(failure) = self.compaction_failure {
            if matches!(failure, MockCompactionFailure::NotDispatched) {
                return BackendCompactionStart::NotDispatched {
                    reason: BackendCompactionNotDispatchedReason::InvalidFocus,
                };
            }
            let (terminal_tx, terminal) = tokio::sync::oneshot::channel();
            let operation_id = request.operation_id;
            let _ = terminal_tx.send(BackendCompactionResult {
                operation_id: operation_id.clone(),
                dispatch: BackendCompactionDispatchState::Rejected,
                mutation: BackendCompactionMutationState::NotObserved,
                outcome: Err(super::BackendCompactionFailure {
                    kind: super::BackendCompactionFailureKind::ProviderRejected,
                    message: "mock native compaction rejected".to_owned(),
                }),
                provider_session_id: Some(self.session_id.clone()),
                metrics: CompactionMetrics::default(),
                post_context_tokens: PostCompactionTokenCount::Unknown,
                evidence: BackendCompactionTerminalEvidence::None,
            });
            return BackendCompactionStart::Accepted(BackendAcceptedCompaction {
                operation_id,
                terminal,
            });
        }
        let (terminal_tx, terminal) = tokio::sync::oneshot::channel();
        let operation_id = request.operation_id.clone();
        *self
            .active_compaction
            .lock()
            .expect("mock active compaction mutex poisoned") = Some(MockCompactionFlight {
            operation_id: operation_id.clone(),
            terminal_tx: Some(terminal_tx),
        });
        let _ = events_tx.send_compaction(BackendCompactionEvent::Progress(
            BackendCompactionProgress {
                operation_id: operation_id.clone(),
                stage: CompactionStage::Dispatching,
                elapsed_ms: Some(0),
            },
        ));

        let active_compaction = Arc::clone(&self.active_compaction);
        let session_id = self.session_id.clone();
        let observation_gates = self.compaction_observation_gates.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = events_tx.send_compaction(BackendCompactionEvent::Progress(
                BackendCompactionProgress {
                    operation_id: operation_id.clone(),
                    stage: CompactionStage::Finalizing,
                    elapsed_ms: Some(1),
                },
            ));
            let terminal_tx = {
                let mut active = active_compaction
                    .lock()
                    .expect("mock active compaction mutex poisoned");
                let Some(flight) = active.as_mut() else {
                    return;
                };
                if flight.operation_id != operation_id {
                    return;
                }
                flight.terminal_tx.take()
            };
            let evidence = if observation_gates.is_some() {
                BackendCompactionTerminalEvidence::Claude {
                    session_id: Some(session_id.0.clone()),
                    boundary_uuid: Some(operation_id.0.clone()),
                    compact_result: None,
                    terminal_result_seen: true,
                }
            } else {
                BackendCompactionTerminalEvidence::None
            };
            let observation_id = evidence.observation_id();
            let result = BackendCompactionResult {
                operation_id: operation_id.clone(),
                dispatch: BackendCompactionDispatchState::Accepted,
                mutation: BackendCompactionMutationState::Completed,
                outcome: Ok(BackendCompactionSuccess {
                    mechanism: CompactionMethod::NativeRpc,
                }),
                provider_session_id: Some(session_id.clone()),
                metrics: CompactionMetrics {
                    before_tokens: Some(12_000),
                    after_tokens: Some(3_000),
                    ..CompactionMetrics::default()
                },
                post_context_tokens: PostCompactionTokenCount::Trusted(3_000),
                evidence,
            };
            if let Some(terminal_tx) = terminal_tx {
                let _ = terminal_tx.send(result);
            }
            {
                let mut active = active_compaction
                    .lock()
                    .expect("mock active compaction mutex poisoned");
                if active
                    .as_ref()
                    .is_some_and(|flight| flight.operation_id == operation_id)
                {
                    *active = None;
                }
            }
            if let Some((release, sent)) = observation_gates {
                release.wait().await;
                let _ = events_tx.send_compaction(BackendCompactionEvent::Observed(Box::new(
                    super::compaction::BackendObservedCompaction {
                        observation_id: observation_id.expect("scripted compaction correlation"),
                        trigger: CompactionTrigger::BackendObservedManual,
                        method: CompactionMethod::NativeRpc,
                        provider_session_id: Some(session_id),
                        metrics: CompactionMetrics {
                            before_tokens: Some(12_000),
                            after_tokens: Some(3_000),
                            ..CompactionMetrics::default()
                        },
                        source:
                            super::compaction::BackendCompactionObservationSource::ClaudeBoundary {
                                boundary_uuid: operation_id.0,
                            },
                        user_focus: None,
                    },
                )));
                sent.wait().await;
            }
        });

        BackendCompactionStart::Accepted(BackendAcceptedCompaction {
            operation_id: request.operation_id,
            terminal,
        })
    }

    async fn send(&self, input: AgentInput) -> bool {
        if self
            .active_compaction
            .lock()
            .expect("mock active compaction mutex poisoned")
            .is_some()
        {
            return false;
        }
        self.command_tx.send(MockCommand::Input(input)).is_ok()
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;
        if self
            .active_compaction
            .lock()
            .expect("mock active compaction mutex poisoned")
            .is_some()
        {
            return SendOutcome::Busy(input);
        }
        if matches!(input, AgentInput::SendMessage(_))
            && self.scripted_busy_self_turn
            && !self
                .busy_self_turn_fired
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = self.command_tx.send(MockCommand::EmitBusySelfTurn);
            return SendOutcome::Busy(input);
        }
        if self.send(input).await {
            SendOutcome::Accepted
        } else {
            SendOutcome::Closed
        }
    }

    async fn interrupt(&self) -> bool {
        self.command_tx.send(MockCommand::Interrupt).is_ok()
    }

    async fn shutdown(mut self) {
        if let Some(gate) = self.shutdown_gate.take() {
            gate.wait().await;
        }
        drop(self.resume_replay_guard.take());
    }
}

fn summarize_startup_mcp_servers(config: &BackendSpawnConfig) -> Vec<String> {
    config
        .startup_mcp_servers
        .iter()
        .map(|server| match &server.transport {
            StartupMcpTransport::Http { .. } => format!("{}(http)", server.name),
            StartupMcpTransport::Stdio { .. } => format!("{}(stdio)", server.name),
        })
        .collect()
}

fn record_prompt(session_id: &SessionId, prompt: &str) -> usize {
    let mut store = session_store()
        .lock()
        .expect("mock backend session store mutex poisoned");
    let Some(record) = store.get_mut(&session_id.0) else {
        return 0;
    };
    record.prompts.push(prompt.to_string());
    record.updated_at_ms = now_ms();
    record.prompts.len().saturating_sub(1)
}

fn emit_resume_history(
    events_tx: &MockEventSender,
    session_id: &SessionId,
    prompts: &[String],
    user_bubbles: bool,
) {
    for (prompt_index, prompt) in prompts.iter().enumerate() {
        if prompt.trim() == "/compact" {
            let _ = events_tx.send_event(emit::user_bubble(prompt));
            let _ = events_tx.send_event(emit::compaction_observation(
                session_id,
                prompt_index,
                CompactionTrigger::UserTyped,
                CompactionMethod::NativeTextCommand,
            ));
            continue;
        }
        if user_bubbles {
            let _ = events_tx.send_event(emit::user_bubble(prompt));
        }
        let content = format!(
            "{}mock backend response to: {prompt}",
            startup_mcp_response_prefix(session_id)
        );
        let _ = events_tx.send_event(emit::message_added(emit::mock_assistant_message(
            Some(ChatMessageId(Uuid::new_v4().to_string())),
            content,
        )));
    }
}

fn mock_prompt_history(session_id: &SessionId) -> Vec<String> {
    let store = session_store()
        .lock()
        .expect("mock backend session store mutex poisoned");
    store
        .get(&session_id.0)
        .map(|record| record.prompts.clone())
        .unwrap_or_default()
}

/// What the backend was handed at launch, echoed back at the front of every
/// ordinary response so tests can assert on delivery.
fn startup_mcp_response_prefix(session_id: &SessionId) -> String {
    let store = session_store()
        .lock()
        .expect("mock backend session store mutex poisoned");
    let Some(record) = store.get(&session_id.0) else {
        return String::new();
    };
    let mut parts = Vec::new();
    if !record.startup_mcp_servers.is_empty() {
        parts.push(format!(
            "[startup_mcp_servers: {}]",
            record.startup_mcp_servers.join(", ")
        ));
    }
    if let Some(instructions) = record.instructions.as_ref() {
        parts.push(format!("[instructions: {}]", summarize_text(instructions)));
    }
    if !record.steering_body.trim().is_empty() {
        parts.push(format!(
            "[steering: {}]",
            summarize_text(&record.steering_body)
        ));
    }
    if !record.skills.is_empty() {
        parts.push(format!("[skills: {}]", record.skills.join(", ")));
    }
    if !matches!(record.tool_policy, ToolPolicy::Unrestricted) {
        parts.push(format!("[tool_policy: {:?}]", record.tool_policy));
    }
    if record.access_mode != BackendAccessMode::Unrestricted {
        parts.push(format!("[access_mode: {:?}]", record.access_mode));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{} ", parts.join(" "))
}

fn summarize_text(text: &str) -> String {
    text.trim().replace('\n', "\\n")
}

fn summarize_skill(skill: &crate::backend::customization::ResolvedSkill) -> String {
    skill.name.clone()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
}

pub(crate) fn disabled_discovery(
    kind: BackendKind,
    context: &super::BackendProbeContext,
) -> Result<super::BackendDiscovery, String> {
    match kind {
        BackendKind::Hermes => Ok(super::BackendDiscovery {
            schema: mock_filtered_model_schema(kind, context),
            launch_profiles: Vec::new(),
        }),
        _ => Err(format!(
            "{kind:?} model discovery is unavailable because backend probing is disabled"
        )),
    }
}

fn mock_filtered_model_schema(
    kind: BackendKind,
    context: &super::BackendProbeContext,
) -> protocol::SessionSettingsSchema {
    let mut schema = super::session_settings_schema_for_backend(kind);
    let disabled = context
        .disabled_providers
        .get(protocol::hermes_config::HERMES_DEFAULT_PROFILE)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let options = [
        ("mock-anthropic", "mock-claude"),
        ("mock-openai", "mock-gpt"),
    ]
    .into_iter()
    .filter(|(provider, _)| !disabled.iter().any(|disabled| disabled == provider))
    .map(|(provider, model)| protocol::SelectOption {
        value: serde_json::json!({"model": model, "provider": provider}).to_string(),
        label: format!("{provider} — {model}"),
    })
    .collect();
    schema.fields.insert(
        0,
        protocol::SessionSettingField {
            key: "model".to_owned(),
            label: "Model".to_owned(),
            description: None,
            use_slider: false,
            select_options_by_setting: None,
            field_type: protocol::SessionSettingFieldType::Select {
                options,
                default: None,
                nullable: true,
            },
        },
    );
    schema
}
