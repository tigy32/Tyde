use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentId, AgentInput, BackendAccessMode, BackendKind, ChatEvent, ChatMessage, ChatMessageId,
    MessageMetadataUpdateData, MessageSender, MessageTokenUsage, ModelInfo, OperationCancelledData,
    OrchestrationAgentOrigin, OrchestrationAgentType, OrchestrationEvent, OrchestrationId,
    OrchestrationPayload, SessionId, StreamEndData, StreamStartData, StreamTextDeltaData,
    TokenUsage, ToolExecutionCompletedData, ToolExecutionResult, ToolPolicy, ToolRequest,
    ToolRequestType,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use super::agent_control_progress::{await_progress_data_for_tool, tyde_tool_result};
use super::empty_session_settings_schema;
use super::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, StartupMcpTransport,
};
use crate::sub_agent::{SubAgentEmitter, SubAgentHandle};

const MOCK_MODEL: &str = "mock";
const FORCE_SPAWN_FAILURE_SENTINEL: &str = "__mock_fail_spawn__";
const EMPTY_AGENT_CONTROL_OUTPUT_SENTINEL: &str = "__mock_empty_agent_control_output__";
const SPAWN_NATIVE_CHILD_SENTINEL: &str = "__mock_spawn_native_child__";
pub(crate) const SPAWN_LIVE_NATIVE_CHILD_SENTINEL: &str = "__mock_spawn_live_native_child__";
/// Like `__mock_spawn_native_child__` but drops the emitter handle immediately
/// after the turn completes, which closes the child's backend event stream.
/// Used to regression-test that the relay agent actor parks instead of exiting
/// when the event stream ends — an exited relay actor with a live registry
/// entry panics the next `snapshot()` call during host-stream replay.
pub(crate) const SPAWN_NATIVE_CHILD_AND_DROP_SENTINEL: &str =
    "__mock_spawn_native_child_and_drop__";
const MOCK_CANCEL_TURN_SENTINEL: &str = "__mock_cancel__";
const MOCK_COMPACT_SENTINEL: &str = "/compact";
/// Causes `emit_turn` to sleep (see `MOCK_SLOW_SLEEP_MS`) before emitting
/// `TypingStatusChanged(false)`.  This gives tests a reliable window to send
/// queued messages while the agent is still in-turn, without relying on
/// wall-clock races.  The window also gives replay tests enough time to
/// connect a second client and verify state.
pub(crate) const MOCK_SLOW_TURN_SENTINEL: &str = "__mock_slow__";
const MOCK_HOLD_UNTIL_INTERRUPT_SENTINEL: &str = "__mock_hold_until_interrupt__";
pub(crate) const MOCK_DUPLICATE_IDLE_SENTINEL: &str = "__mock_duplicate_idle__";
/// Causes the mock backend task to emit `TypingStatusChanged(true)`, sleep 300 ms,
/// then exit without completing the turn.  The events channel closes when the
/// task exits, which drives the agent actor into `enter_terminal_failure`.
pub(crate) const MOCK_DIE_AFTER_BUSY_SENTINEL: &str = "__mock_die_after_busy__";
pub(crate) const MOCK_ERROR_WITHOUT_IDLE_SENTINEL: &str = "__mock_error_without_idle__";
/// Makes the FIRST send of a message containing this sentinel return
/// `SendOutcome::Busy` (handing the message back) while the mock runs a turn
/// "it started on its own"; subsequent sends are accepted normally. Simulates
/// a backend that resumed on its own initiative at the moment of a send.
pub(crate) const MOCK_BUSY_SELF_TURN_SENTINEL: &str = "__mock_busy_self_turn__";
/// Emits a diagnostic Error card in the middle of an otherwise normal
/// streaming turn (typing stays on and the stream closes properly afterward).
/// Exercises the agent rule that a mid-turn error must not end the turn.
pub(crate) const MOCK_MID_TURN_ERROR_SENTINEL: &str = "__mock_mid_turn_error__";
pub(crate) const MOCK_TOOL_FAILURE_WITHOUT_IDLE_SENTINEL: &str =
    "__mock_tool_failure_without_idle__";
const MOCK_EXIT_PLAN_MODE_SENTINEL: &str = "__mock_exit_plan_mode__";
const MOCK_EXIT_PLAN_MODE_STREAM_END_FIRST_SENTINEL: &str =
    "__mock_exit_plan_mode_stream_end_first__";
const MOCK_AGENT_CONTROL_AWAIT_SENTINEL: &str = "__mock_agent_control_await__";
pub(crate) const MOCK_AGENT_CONTROL_SEND_MESSAGE_SENTINEL: &str =
    "__mock_agent_control_send_message__";
const MOCK_HISTORY_SENTINEL: &str = "__mock_history__";
const MOCK_CLOSE_RESUME_BEFORE_BARRIER_SENTINEL: &str = "__mock_close_resume_before_barrier__";
const MOCK_LATE_USAGE_SENTINEL: &str = "__mock_late_usage__";
const MOCK_NO_USAGE_SENTINEL: &str = "__mock_no_usage__";
const MOCK_ORCHESTRATION_SENTINEL: &str = "__mock_orchestration__";
const MOCK_FAILED_TOOL_CALL_ID: &str = "mock-failed-tool";
const MOCK_EXIT_PLAN_MODE_TOOL_CALL_ID: &str = "mock-exit-plan-tool";
const MOCK_EXIT_PLAN_MODE_PLAN: &str = "# Plan\n\nApprove the mock plan.";
const MOCK_EXIT_PLAN_MODE_PLAN_PATH: &str = "/tmp/mock/.claude/plans/mock-plan.md";
/// Sleep for __mock_slow__ turns — long enough for replay tests to connect a
/// second client and see the queued-message snapshot before the turn ends.
///
/// Sized against two bounds: it must exceed the time for a second client to
/// connect and replay the agent bootstrap (~1 s, and higher under load), yet
/// stay under the 5 s per-event `next_event` timeout used by tests that wait
/// for the slow turn to finish — the trailing `TypingStatusChanged(false)` is
/// emitted only after this sleep. 4 s sits comfortably between the two.
const MOCK_SLOW_SLEEP_MS: u64 = 4_000;
/// Sleep for __mock_die_after_busy__ — just enough for tests to queue messages.
const MOCK_DIE_SLEEP_MS: u64 = 300;
const MOCK_EXIT_PLAN_MODE_RESUME_DELAY_MS: u64 = 1_000;
const MOCK_RESUME_REPLAY_DELAY_MS: u64 = 25;

struct PendingExitPlanMode {
    tool_call_id: String,
    delay_before_completion: Duration,
    delay_after_completion: Duration,
}

#[derive(Debug, Clone)]
struct MockSessionRecord {
    workspace_roots: Vec<String>,
    prompts: Vec<String>,
    startup_mcp_servers: Vec<String>,
    instructions: Option<String>,
    steering_body: String,
    skills: Vec<String>,
    tool_policy: ToolPolicy,
    access_mode: BackendAccessMode,
    created_at_ms: u64,
    updated_at_ms: u64,
}

fn session_store() -> &'static Mutex<HashMap<String, MockSessionRecord>> {
    static STORE: OnceLock<Mutex<HashMap<String, MockSessionRecord>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mock_turn_token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1250,
        output_tokens: 340,
        total_tokens: 1590,
        cached_prompt_tokens: Some(800),
        cache_creation_input_tokens: Some(50),
        reasoning_tokens: Some(120),
    }
}

#[derive(Clone)]
struct MockAgentControlAwaitMcp {
    url: String,
    authorization: Option<String>,
}

fn agent_control_await_mcp(
    startup_mcp_servers: &[StartupMcpServer],
) -> Option<MockAgentControlAwaitMcp> {
    startup_mcp_servers
        .iter()
        .find(|server| server.name == crate::agent_control_mcp::AGENT_CONTROL_AWAIT_MCP_SERVER_NAME)
        .and_then(|server| match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => Some(MockAgentControlAwaitMcp {
                url: url.clone(),
                authorization: headers
                    .get(axum::http::header::AUTHORIZATION.as_str())
                    .cloned(),
            }),
            StartupMcpTransport::Stdio { .. } => None,
        })
}

pub struct MockBackend {
    command_tx: mpsc::UnboundedSender<MockCommand>,
    session_id: SessionId,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
    busy_self_turn_fired: Arc<std::sync::atomic::AtomicBool>,
}

enum MockCommand {
    Input(AgentInput),
    Interrupt,
    EmitBusySelfTurn,
}

struct MockLoopConfig {
    initial_message: Option<String>,
    slow_initial_turn: bool,
    hold_initial_turn: bool,
    agent_control_await_mcp: Option<MockAgentControlAwaitMcp>,
}

impl MockBackend {
    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        let _ = self.subagent_emitter_tx.send(Some(emitter));
    }
}

impl Backend for MockBackend {
    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Claude)
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        if initial_message.contains(FORCE_SPAWN_FAILURE_SENTINEL) {
            return Err("mock backend forced spawn failure".to_string());
        }
        let agent_control_await_mcp = agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = config
            .startup_mcp_servers
            .iter()
            .map(|server| match &server.transport {
                StartupMcpTransport::Http { .. } => format!("{}(http)", server.name),
                StartupMcpTransport::Stdio { .. } => format!("{}(stdio)", server.name),
            })
            .collect::<Vec<_>>();
        let session_id = SessionId(Uuid::new_v4().to_string());
        let now = now_ms();
        let resolved_spawn_config = config.resolved_spawn_config.clone();
        let slow_initial_turn = resolved_spawn_config
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains(MOCK_SLOW_TURN_SENTINEL));
        let hold_initial_turn = resolved_spawn_config
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains(MOCK_HOLD_UNTIL_INTERRUPT_SENTINEL));

        {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            store.insert(
                session_id.0.clone(),
                MockSessionRecord {
                    workspace_roots,
                    prompts: Vec::new(),
                    startup_mcp_servers: startup_mcp_servers.clone(),
                    instructions: resolved_spawn_config.instructions,
                    steering_body: resolved_spawn_config.steering_body,
                    skills: resolved_spawn_config
                        .skills
                        .into_iter()
                        .map(|skill| format!("{}={}", skill.name, summarize_text(&skill.body)))
                        .collect(),
                    tool_policy: resolved_spawn_config.tool_policy,
                    access_mode: resolved_spawn_config.access_mode,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
        }

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (subagent_emitter_tx, subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let session_id_for_task = session_id.clone();

        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx,
            subagent_emitter_rx,
            MockLoopConfig {
                initial_message: Some(initial_message),
                slow_initial_turn,
                hold_initial_turn,
                agent_control_await_mcp,
            },
        );

        Ok((
            Self {
                command_tx,
                session_id,
                subagent_emitter_tx,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let agent_control_await_mcp = agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = config
            .startup_mcp_servers
            .iter()
            .map(|server| match &server.transport {
                StartupMcpTransport::Http { .. } => format!("{}(http)", server.name),
                StartupMcpTransport::Stdio { .. } => format!("{}(stdio)", server.name),
            })
            .collect::<Vec<_>>();
        let resolved_spawn_config = config.resolved_spawn_config.clone();
        let replay_prompts = {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            let Some(record) = store.get_mut(&session_id.0) else {
                return Err(format!("unknown mock session {}", session_id.0));
            };
            let replay_prompts = record.prompts.clone();
            record.workspace_roots = workspace_roots;
            record.startup_mcp_servers = startup_mcp_servers;
            record.instructions = resolved_spawn_config.instructions;
            record.steering_body = resolved_spawn_config.steering_body;
            record.skills = resolved_spawn_config
                .skills
                .into_iter()
                .map(|skill| format!("{}={}", skill.name, summarize_text(&skill.body)))
                .collect();
            record.tool_policy = resolved_spawn_config.tool_policy;
            record.access_mode = resolved_spawn_config.access_mode;
            record.updated_at_ms = now_ms();
            replay_prompts
        };

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let (subagent_emitter_tx, subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let session_id_for_task = session_id.clone();

        if replay_prompts
            .iter()
            .any(|prompt| prompt.contains(MOCK_CLOSE_RESUME_BEFORE_BARRIER_SENTINEL))
        {
            tokio::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                drop(events_tx);
                sleep(Duration::from_secs(5)).await;
                drop(resume_replay_complete_tx);
            });
            return Ok((
                Self {
                    command_tx,
                    session_id,
                    subagent_emitter_tx,
                    busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
                EventStream::new_with_resume_replay_barrier(events_rx, resume_replay_complete_rx),
            ));
        }

        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx.clone(),
            subagent_emitter_rx,
            MockLoopConfig {
                initial_message: None,
                slow_initial_turn: false,
                hold_initial_turn: false,
                agent_control_await_mcp,
            },
        );

        let replay_session_id = session_id.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(MOCK_RESUME_REPLAY_DELAY_MS)).await;
            emit_mock_resume_history(&events_tx, &replay_session_id, &replay_prompts);
            let _ = resume_replay_complete_tx.send(());
        });

        Ok((
            Self {
                command_tx,
                session_id,
                subagent_emitter_tx,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            EventStream::new_with_resume_replay_barrier(events_rx, resume_replay_complete_rx),
        ))
    }

    async fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        let initial_message = initial_input.message;
        let agent_control_await_mcp = agent_control_await_mcp(&config.startup_mcp_servers);
        let startup_mcp_servers = config
            .startup_mcp_servers
            .iter()
            .map(|server| match &server.transport {
                StartupMcpTransport::Http { .. } => format!("{}(http)", server.name),
                StartupMcpTransport::Stdio { .. } => format!("{}(stdio)", server.name),
            })
            .collect::<Vec<_>>();
        let session_id = SessionId(Uuid::new_v4().to_string());
        let now = now_ms();
        let resolved_spawn_config = config.resolved_spawn_config.clone();

        {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            let Some(source) = store.get(&from_session_id.0).cloned() else {
                return Err(BackendStartupError::backend_failed(format!(
                    "unknown mock session {}",
                    from_session_id.0
                )));
            };
            store.insert(
                session_id.0.clone(),
                MockSessionRecord {
                    workspace_roots,
                    prompts: source.prompts,
                    startup_mcp_servers,
                    instructions: resolved_spawn_config.instructions,
                    steering_body: resolved_spawn_config.steering_body,
                    skills: resolved_spawn_config
                        .skills
                        .into_iter()
                        .map(|skill| format!("{}={}", skill.name, summarize_text(&skill.body)))
                        .collect(),
                    tool_policy: resolved_spawn_config.tool_policy,
                    access_mode: resolved_spawn_config.access_mode,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
        }

        let (command_tx, command_rx) = mpsc::unbounded_channel::<MockCommand>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (subagent_emitter_tx, subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let session_id_for_task = session_id.clone();
        start_mock_command_loop(
            session_id_for_task,
            command_rx,
            events_tx,
            subagent_emitter_rx,
            MockLoopConfig {
                initial_message: Some(initial_message),
                slow_initial_turn: false,
                hold_initial_turn: false,
                agent_control_await_mcp,
            },
        );

        Ok((
            Self {
                command_tx,
                session_id,
                subagent_emitter_tx,
                busy_self_turn_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            EventStream::new(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
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

    async fn send(&self, input: AgentInput) -> bool {
        self.command_tx.send(MockCommand::Input(input)).is_ok()
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;
        if let AgentInput::SendMessage(payload) = &input
            && payload.message.contains(MOCK_BUSY_SELF_TURN_SENTINEL)
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

    async fn shutdown(self) {
        drop(self);
    }
}

fn start_mock_command_loop(
    session_id_for_task: SessionId,
    mut command_rx: mpsc::UnboundedReceiver<MockCommand>,
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    mut subagent_emitter_rx: watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    config: MockLoopConfig,
) {
    let MockLoopConfig {
        initial_message,
        slow_initial_turn,
        hold_initial_turn,
        agent_control_await_mcp,
    } = config;
    tokio::spawn(async move {
        let mut active_subagents = Vec::new();
        let mut pending_exit_plan_mode = None;
        let mut holding_until_interrupt = false;
        if let Some(initial_message) = initial_message {
            if initial_message.contains(MOCK_DIE_AFTER_BUSY_SENTINEL) {
                // Send TypingStatusChanged(true) so the actor sets in_turn=true,
                // then sleep to give tests time to queue messages, then return so
                // that events_tx is dropped and the actor detects termination.
                let _ = events_tx.send(ChatEvent::TypingStatusChanged(true));
                sleep(Duration::from_millis(MOCK_DIE_SLEEP_MS)).await;
                return;
            }
            record_prompt(&session_id_for_task, &initial_message);
            if hold_initial_turn || initial_message.contains(MOCK_HOLD_UNTIL_INTERRUPT_SENTINEL) {
                if !emit_held_turn(&events_tx, &session_id_for_task, &initial_message).await {
                    return;
                }
                maybe_spawn_live_native_child(
                    &initial_message,
                    &mut subagent_emitter_rx,
                    &mut active_subagents,
                )
                .await;
                holding_until_interrupt = true;
            } else if let Some((agent_id, message)) =
                parse_mock_agent_control_send_message(&initial_message)
            {
                if !emit_mock_agent_control_send_message(&events_tx, agent_id, message) {
                    return;
                }
            } else if let Some(agent_ids) = parse_mock_agent_control_await(&initial_message) {
                if !emit_mock_agent_control_await(
                    &events_tx,
                    agent_control_await_mcp.as_ref(),
                    agent_ids,
                )
                .await
                {
                    return;
                }
            } else if initial_message.contains(MOCK_EXIT_PLAN_MODE_SENTINEL)
                || initial_message.contains(MOCK_EXIT_PLAN_MODE_STREAM_END_FIRST_SENTINEL)
            {
                let stream_end_before_request =
                    initial_message.contains(MOCK_EXIT_PLAN_MODE_STREAM_END_FIRST_SENTINEL);
                if emit_exit_plan_mode_request(&events_tx, stream_end_before_request)
                    .await
                    .is_none()
                {
                    return;
                }
                pending_exit_plan_mode = Some(PendingExitPlanMode {
                    tool_call_id: MOCK_EXIT_PLAN_MODE_TOOL_CALL_ID.to_owned(),
                    delay_before_completion: if stream_end_before_request {
                        Duration::from_millis(MOCK_EXIT_PLAN_MODE_RESUME_DELAY_MS)
                    } else {
                        Duration::ZERO
                    },
                    delay_after_completion: if stream_end_before_request {
                        Duration::from_millis(MOCK_EXIT_PLAN_MODE_RESUME_DELAY_MS)
                    } else {
                        Duration::ZERO
                    },
                });
            } else if initial_message.contains(MOCK_ERROR_WITHOUT_IDLE_SENTINEL) {
                emit_mock_error(&events_tx, "mock backend emitted error without idle");
            } else if initial_message.contains(MOCK_TOOL_FAILURE_WITHOUT_IDLE_SENTINEL) {
                emit_mock_tool_failure_without_idle(&events_tx);
            } else if initial_message.contains(MOCK_ORCHESTRATION_SENTINEL) {
                emit_mock_orchestration(&events_tx);
            } else {
                if !emit_turn(
                    &events_tx,
                    &session_id_for_task,
                    &initial_message,
                    slow_initial_turn,
                )
                .await
                {
                    return;
                }
                maybe_spawn_native_child(
                    &initial_message,
                    &mut subagent_emitter_rx,
                    &mut active_subagents,
                )
                .await;
            }
        }

        while let Some(command) = command_rx.recv().await {
            match command {
                MockCommand::Input(AgentInput::SendMessage(payload)) => {
                    if holding_until_interrupt {
                        emit_mock_error(
                            &events_tx,
                            "mock backend received input while holding until interrupt",
                        );
                        continue;
                    }
                    if let Some(tool_response) = payload.tool_response {
                        handle_exit_plan_mode_tool_response(
                            &events_tx,
                            &session_id_for_task,
                            &mut pending_exit_plan_mode,
                            tool_response,
                        )
                        .await;
                        continue;
                    }
                    if pending_exit_plan_mode.is_some() {
                        emit_mock_error(
                            &events_tx,
                            "mock backend received normal input while ExitPlanMode is pending",
                        );
                        continue;
                    }
                    record_prompt(&session_id_for_task, &payload.message);
                    if let Some((agent_id, message)) =
                        parse_mock_agent_control_send_message(&payload.message)
                    {
                        if !emit_mock_agent_control_send_message(&events_tx, agent_id, message) {
                            return;
                        }
                    } else if let Some(agent_ids) = parse_mock_agent_control_await(&payload.message)
                    {
                        if !emit_mock_agent_control_await(
                            &events_tx,
                            agent_control_await_mcp.as_ref(),
                            agent_ids,
                        )
                        .await
                        {
                            return;
                        }
                    } else if payload.message.contains(MOCK_EXIT_PLAN_MODE_SENTINEL)
                        || payload
                            .message
                            .contains(MOCK_EXIT_PLAN_MODE_STREAM_END_FIRST_SENTINEL)
                    {
                        let stream_end_before_request = payload
                            .message
                            .contains(MOCK_EXIT_PLAN_MODE_STREAM_END_FIRST_SENTINEL);
                        if emit_exit_plan_mode_request(&events_tx, stream_end_before_request)
                            .await
                            .is_none()
                        {
                            return;
                        }
                        pending_exit_plan_mode = Some(PendingExitPlanMode {
                            tool_call_id: MOCK_EXIT_PLAN_MODE_TOOL_CALL_ID.to_owned(),
                            delay_before_completion: if stream_end_before_request {
                                Duration::from_millis(MOCK_EXIT_PLAN_MODE_RESUME_DELAY_MS)
                            } else {
                                Duration::ZERO
                            },
                            delay_after_completion: if stream_end_before_request {
                                Duration::from_millis(MOCK_EXIT_PLAN_MODE_RESUME_DELAY_MS)
                            } else {
                                Duration::ZERO
                            },
                        });
                    } else if payload.message.contains(MOCK_DIE_AFTER_BUSY_SENTINEL) {
                        let _ = events_tx.send(ChatEvent::TypingStatusChanged(true));
                        sleep(Duration::from_millis(MOCK_DIE_SLEEP_MS)).await;
                        return;
                    } else if payload.message.contains(MOCK_ERROR_WITHOUT_IDLE_SENTINEL) {
                        emit_mock_error(&events_tx, "mock backend emitted error without idle");
                    } else if payload
                        .message
                        .contains(MOCK_TOOL_FAILURE_WITHOUT_IDLE_SENTINEL)
                    {
                        emit_mock_tool_failure_without_idle(&events_tx);
                    } else if payload.message.contains(MOCK_ORCHESTRATION_SENTINEL) {
                        emit_mock_orchestration(&events_tx);
                    } else {
                        if !emit_turn(&events_tx, &session_id_for_task, &payload.message, false)
                            .await
                        {
                            return;
                        }
                        maybe_spawn_native_child(
                            &payload.message,
                            &mut subagent_emitter_rx,
                            &mut active_subagents,
                        )
                        .await;
                    }
                }
                MockCommand::Input(AgentInput::UpdateSessionSettings(_)) => {}
                MockCommand::Input(AgentInput::EditQueuedMessage(_))
                | MockCommand::Input(AgentInput::CancelQueuedMessage(_))
                | MockCommand::Input(AgentInput::SendQueuedMessageNow(_)) => {
                    panic!(
                        "queued-message inputs must be handled by the agent actor before reaching the backend"
                    );
                }
                MockCommand::EmitBusySelfTurn => {
                    if !emit_turn(
                        &events_tx,
                        &session_id_for_task,
                        "self-initiated wakeup",
                        false,
                    )
                    .await
                    {
                        return;
                    }
                }
                MockCommand::Interrupt => {
                    if holding_until_interrupt {
                        for child in &active_subagents {
                            let _ = child.event_tx.send(ChatEvent::StreamEnd(StreamEndData {
                                message: ChatMessage {
                                    message_id: Some(ChatMessageId(format!(
                                        "mock-live-{}",
                                        child.agent_id.0
                                    ))),
                                    timestamp: now_ms(),
                                    sender: MessageSender::Assistant {
                                        agent: "mock-live-native-child".to_owned(),
                                    },
                                    content: "mock live native child working".to_owned(),
                                    reasoning: None,
                                    tool_calls: Vec::new(),
                                    model_info: Some(ModelInfo {
                                        model: MOCK_MODEL.to_owned(),
                                    }),
                                    token_usage: None,
                                    context_breakdown: None,
                                    images: None,
                                },
                            }));
                            let _ = child.event_tx.send(ChatEvent::OperationCancelled(
                                OperationCancelledData {
                                    message:
                                        "Parent agent turn ended before the sub-agent completed"
                                            .to_owned(),
                                },
                            ));
                            let _ = child.event_tx.send(ChatEvent::TypingStatusChanged(false));
                        }
                        let _ =
                            events_tx.send(ChatEvent::OperationCancelled(OperationCancelledData {
                                message: "mock backend interrupted held turn".to_owned(),
                            }));
                        let _ = events_tx.send(ChatEvent::TypingStatusChanged(false));
                        holding_until_interrupt = false;
                        continue;
                    }
                    break;
                }
            }
        }

        drop(active_subagents);
    });
}

async fn maybe_spawn_live_native_child(
    prompt: &str,
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    active_subagents: &mut Vec<SubAgentHandle>,
) {
    if !prompt.contains(SPAWN_LIVE_NATIVE_CHILD_SENTINEL) {
        return;
    }
    sleep(Duration::from_millis(50)).await;
    let emitter = wait_for_subagent_emitter(subagent_emitter_rx).await;
    let handle = match emitter
        .on_subagent_spawned(
            format!("mock-live-tool-use-{}", Uuid::new_v4()),
            "mock-live-native-child".to_owned(),
            "live native child task".to_owned(),
            "mock".to_owned(),
            Some(SessionId(Uuid::new_v4().to_string())),
        )
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            tracing::error!(%error, "mock live native child relay registration failed");
            return;
        }
    };
    let message_id = Some(format!("mock-live-{}", handle.agent_id.0));
    let _ = handle.event_tx.send(ChatEvent::TypingStatusChanged(true));
    let _ = handle
        .event_tx
        .send(ChatEvent::StreamStart(StreamStartData {
            message_id: message_id.clone(),
            agent: "mock-live-native-child".to_owned(),
            model: Some(MOCK_MODEL.to_owned()),
        }));
    let _ = handle
        .event_tx
        .send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id,
            text: "mock live native child working".to_owned(),
        }));
    active_subagents.push(handle);
}

async fn emit_held_turn(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id: &SessionId,
    user_message: &str,
) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = format!(
        "{}mock backend held response to: {user_message}",
        startup_mcp_response_prefix(session_id)
    );

    events_tx.send(ChatEvent::TypingStatusChanged(true)).is_ok()
        && events_tx
            .send(ChatEvent::StreamStart(StreamStartData {
                message_id: message_id.clone(),
                agent: "mock".to_owned(),
                model: Some(MOCK_MODEL.to_owned()),
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: message_id.clone(),
                text: response_text.clone(),
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    message_id: message_id.map(protocol::ChatMessageId),
                    timestamp: now_ms(),
                    sender: MessageSender::Assistant {
                        agent: "mock".to_owned(),
                    },
                    content: response_text,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: Some(ModelInfo {
                        model: MOCK_MODEL.to_owned(),
                    }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
            .is_ok()
}

fn record_prompt(session_id: &SessionId, prompt: &str) {
    let mut store = session_store()
        .lock()
        .expect("mock backend session store mutex poisoned");
    let Some(record) = store.get_mut(&session_id.0) else {
        return;
    };
    record.prompts.push(prompt.to_string());
    record.updated_at_ms = now_ms();
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

fn emit_mock_resume_history(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id: &SessionId,
    prompts: &[String],
) {
    for prompt in prompts {
        let content = format!(
            "{}mock backend response to: {prompt}",
            startup_mcp_response_prefix(session_id)
        );
        let _ = events_tx.send(ChatEvent::MessageAdded(ChatMessage {
            message_id: Some(protocol::ChatMessageId(Uuid::new_v4().to_string())),
            timestamp: now_ms(),
            sender: MessageSender::Assistant {
                agent: "mock".to_owned(),
            },
            content,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: Some(ModelInfo {
                model: MOCK_MODEL.to_owned(),
            }),
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
    }
}

async fn emit_turn(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id: &SessionId,
    user_message: &str,
    force_slow: bool,
) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = if user_message.contains(MOCK_HISTORY_SENTINEL) {
        format!(
            "mock history: {}",
            mock_prompt_history(session_id).join(" | ")
        )
    } else {
        format!(
            "{}mock backend response to: {user_message}",
            startup_mcp_response_prefix(session_id)
        )
    };

    if events_tx
        .send(ChatEvent::TypingStatusChanged(true))
        .is_err()
    {
        return false;
    }

    if user_message.contains(MOCK_CANCEL_TURN_SENTINEL) {
        if events_tx
            .send(ChatEvent::OperationCancelled(OperationCancelledData {
                message: format!("mock backend cancelled: {user_message}"),
            }))
            .is_err()
        {
            return false;
        }
        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .is_ok();
    }

    if user_message.contains(EMPTY_AGENT_CONTROL_OUTPUT_SENTINEL) {
        let empty_output_message_id = ChatMessageId(Uuid::new_v4().to_string());
        if events_tx
            .send(ChatEvent::StreamStart(StreamStartData {
                message_id: Some(empty_output_message_id.0.clone()),
                agent: "mock".to_owned(),
                model: None,
            }))
            .is_err()
        {
            return false;
        }
        if events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    message_id: Some(empty_output_message_id),
                    timestamp: now_ms(),
                    sender: MessageSender::Assistant {
                        agent: "mock".to_owned(),
                    },
                    content: String::new(),
                    reasoning: Some(protocol::ReasoningData {
                        text: "hidden reasoning".to_owned(),
                        tokens: None,
                        signature: None,
                        blob: None,
                    }),
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
            .is_err()
        {
            return false;
        }
        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .is_ok();
    }

    if user_message.trim() == MOCK_COMPACT_SENTINEL {
        let compact_message_id = ChatMessageId(Uuid::new_v4().to_string());
        if events_tx
            .send(ChatEvent::StreamStart(StreamStartData {
                message_id: Some(compact_message_id.0.clone()),
                agent: "mock".to_owned(),
                model: Some(MOCK_MODEL.to_owned()),
            }))
            .is_err()
        {
            return false;
        }

        if events_tx
            .send(ChatEvent::MessageAdded(ChatMessage {
                message_id: None,
                timestamp: now_ms(),
                sender: MessageSender::System,
                content: "Conversation compacted.".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            }))
            .is_err()
        {
            return false;
        }

        if events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    message_id: Some(compact_message_id),
                    timestamp: now_ms(),
                    sender: MessageSender::Assistant {
                        agent: "mock".to_owned(),
                    },
                    content: String::new(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: Some(ModelInfo {
                        model: MOCK_MODEL.to_owned(),
                    }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
            .is_err()
        {
            return false;
        }

        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .is_ok();
    }

    if events_tx
        .send(ChatEvent::StreamStart(StreamStartData {
            message_id: message_id.clone(),
            agent: "mock".to_owned(),
            model: Some(MOCK_MODEL.to_owned()),
        }))
        .is_err()
    {
        return false;
    }

    if events_tx
        .send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: message_id.clone(),
            text: response_text.clone(),
        }))
        .is_err()
    {
        return false;
    }

    if user_message.contains(MOCK_MID_TURN_ERROR_SENTINEL) {
        emit_mock_error(events_tx, "mock mid-turn diagnostic error");
    }

    let delayed_usage = user_message.contains(MOCK_LATE_USAGE_SENTINEL);
    let omit_usage = user_message.contains(MOCK_NO_USAGE_SENTINEL);
    let message_id_for_metadata = message_id.clone();
    let message = ChatMessage {
        message_id: message_id.clone().map(ChatMessageId),
        timestamp: now_ms(),
        sender: MessageSender::Assistant {
            agent: "mock".to_owned(),
        },
        content: response_text,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: Some(ModelInfo {
            model: MOCK_MODEL.to_owned(),
        }),
        token_usage: (!delayed_usage && !omit_usage).then(|| {
            MessageTokenUsage::request_and_turn_known(
                mock_turn_token_usage(),
                mock_turn_token_usage(),
            )
        }),
        context_breakdown: None,
        images: None,
    };

    if events_tx
        .send(ChatEvent::StreamEnd(StreamEndData { message }))
        .is_err()
    {
        return false;
    }

    if delayed_usage
        && let Some(message_id) = message_id_for_metadata
        && events_tx
            .send(ChatEvent::MessageMetadataUpdated(
                MessageMetadataUpdateData {
                    message_id: ChatMessageId(message_id),
                    model_info: None,
                    token_usage: Some(MessageTokenUsage::request_and_turn_known(
                        mock_turn_token_usage(),
                        mock_turn_token_usage(),
                    )),
                    context_breakdown: None,
                },
            ))
            .is_err()
    {
        return false;
    }

    if force_slow || user_message.contains(MOCK_SLOW_TURN_SENTINEL) {
        // Yield here so the Tokio scheduler can run client tasks and allow tests
        // to send queued messages before the turn officially ends.
        sleep(Duration::from_millis(MOCK_SLOW_SLEEP_MS)).await;
    }

    if events_tx
        .send(ChatEvent::TypingStatusChanged(false))
        .is_err()
    {
        return false;
    }

    if user_message.contains(MOCK_DUPLICATE_IDLE_SENTINEL) {
        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .is_ok();
    }

    true
}

fn parse_mock_agent_control_await(message: &str) -> Option<Vec<String>> {
    let (_, after_sentinel) = message.split_once(MOCK_AGENT_CONTROL_AWAIT_SENTINEL)?;
    let agent_ids = after_sentinel
        .split_whitespace()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    (!agent_ids.is_empty()).then_some(agent_ids)
}

fn parse_mock_agent_control_send_message(message: &str) -> Option<(AgentId, String)> {
    let (_, after_sentinel) = message.split_once(MOCK_AGENT_CONTROL_SEND_MESSAGE_SENTINEL)?;
    let after_sentinel = after_sentinel.trim_start();
    let (agent_id, message) = after_sentinel.split_once(char::is_whitespace)?;
    let message = message.trim_start();
    (!agent_id.is_empty() && !message.is_empty())
        .then(|| (AgentId(agent_id.to_owned()), message.to_owned()))
}

fn emit_mock_agent_control_send_message(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    agent_id: AgentId,
    message: String,
) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let tool_call_id = format!("mock-agent-control-send-message-{}", Uuid::new_v4());
    let tool_name = "tyde_send_agent_message";
    let response_text = "mock agent-control message delivered".to_owned();

    events_tx.send(ChatEvent::TypingStatusChanged(true)).is_ok()
        && events_tx
            .send(ChatEvent::StreamStart(StreamStartData {
                message_id: message_id.clone(),
                agent: "mock".to_owned(),
                model: Some(MOCK_MODEL.to_owned()),
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.to_owned(),
                tool_type: ToolRequestType::TydeSendAgentMessage { agent_id, message },
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name: tool_name.to_owned(),
                    tool_result: ToolExecutionResult::TydeSendAgentMessage,
                    success: true,
                    error: None,
                    normalization_failure: None,
                },
            ))
            .is_ok()
        && events_tx
            .send(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: message_id.clone(),
                text: response_text.clone(),
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    message_id: message_id.map(ChatMessageId),
                    timestamp: now_ms(),
                    sender: MessageSender::Assistant {
                        agent: "mock".to_owned(),
                    },
                    content: response_text,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: Some(ModelInfo {
                        model: MOCK_MODEL.to_owned(),
                    }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
            .is_ok()
        && events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .is_ok()
}

async fn emit_mock_agent_control_await(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    agent_control_await_mcp: Option<&MockAgentControlAwaitMcp>,
    agent_ids: Vec<String>,
) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let tool_call_id = format!("mock-agent-control-await-{}", Uuid::new_v4());
    let tool_name = "tyde_await_agents";
    let typed_agent_ids = agent_ids.iter().cloned().map(AgentId).collect::<Vec<_>>();
    let arguments = json!({ "agent_ids": agent_ids.clone() });

    if events_tx
        .send(ChatEvent::TypingStatusChanged(true))
        .is_err()
    {
        return false;
    }
    if events_tx
        .send(ChatEvent::StreamStart(StreamStartData {
            message_id: message_id.clone(),
            agent: "mock".to_owned(),
            model: Some(MOCK_MODEL.to_owned()),
        }))
        .is_err()
    {
        return false;
    }
    if events_tx
        .send(ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.to_owned(),
            tool_type: ToolRequestType::TydeAwaitAgents {
                agent_ids: typed_agent_ids,
            },
        }))
        .is_err()
    {
        return false;
    }
    if let Some(progress) = await_progress_data_for_tool(&tool_call_id, tool_name, &arguments)
        && events_tx.send(ChatEvent::ToolProgress(progress)).is_err()
    {
        return false;
    }

    let result = match agent_control_await_mcp {
        Some(config) => call_agent_control_await_mcp(config, &agent_ids).await,
        None => Err("mock backend has no tyde-agent-await MCP server".to_owned()),
    };
    let (success, tool_result, error, response_text, normalization_failure) = match result {
        Ok(body) => match tyde_tool_result(tool_name, &body) {
            Ok(Some(tool_result)) => (
                true,
                tool_result,
                None,
                format!("mock agent-control await completed: {body}"),
                None,
            ),
            Ok(None) => unreachable!("canonical await tool must normalize"),
            Err(error) => (
                false,
                ToolExecutionResult::Error {
                    short_message: "tyde_await_agents result normalization failed".to_owned(),
                    detailed_message: error.to_string(),
                },
                Some(error.to_string()),
                format!("mock agent-control await normalization failed: {error}"),
                Some(error.normalization_failure),
            ),
        },
        Err(error) => (
            false,
            ToolExecutionResult::Error {
                short_message: "tyde_await_agents failed".to_owned(),
                detailed_message: error.clone(),
            },
            Some(error.clone()),
            format!("mock agent-control await failed: {error}"),
            None,
        ),
    };

    if events_tx
        .send(ChatEvent::ToolExecutionCompleted(
            ToolExecutionCompletedData {
                tool_call_id,
                tool_name: tool_name.to_owned(),
                tool_result,
                success,
                error,
                normalization_failure,
            },
        ))
        .is_err()
    {
        return false;
    }
    if events_tx
        .send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: message_id.clone(),
            text: response_text.clone(),
        }))
        .is_err()
    {
        return false;
    }
    if events_tx
        .send(ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: message_id.map(protocol::ChatMessageId),
                timestamp: now_ms(),
                sender: MessageSender::Assistant {
                    agent: "mock".to_owned(),
                },
                content: response_text,
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: Some(ModelInfo {
                    model: MOCK_MODEL.to_owned(),
                }),
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        }))
        .is_err()
    {
        return false;
    }
    events_tx
        .send(ChatEvent::TypingStatusChanged(false))
        .is_ok()
}

async fn call_agent_control_await_mcp(
    config: &MockAgentControlAwaitMcp,
    agent_ids: &[String],
) -> Result<Value, String> {
    let response = post_mcp_json(
        config,
        &json!({
            "jsonrpc": "2.0",
            "id": "mock-agent-control-await",
            "method": "tools/call",
            "params": {
                "name": "tyde_await_agents",
                "arguments": {
                    "agent_ids": agent_ids,
                }
            }
        }),
    )
    .await?;
    let result = response
        .get("result")
        .ok_or_else(|| format!("MCP response missing result: {response}"))?;
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("MCP result missing isError: {response}"))?;
    if is_error {
        return Err(format!("MCP tool call failed: {response}"));
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("MCP response missing content text: {response}"))?;
    serde_json::from_str(text).map_err(|err| format!("failed to parse MCP result JSON: {err}"))
}

async fn post_mcp_json(config: &MockAgentControlAwaitMcp, body: &Value) -> Result<Value, String> {
    let (addr, target) = parse_http_url(&config.url)?;
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|err| format!("connect {addr} failed: {err}"))?;
    let body_bytes =
        serde_json::to_vec(body).map_err(|err| format!("serialize MCP request failed: {err}"))?;
    let authorization = config
        .authorization
        .as_ref()
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\n{authorization}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| format!("write MCP HTTP headers failed: {err}"))?;
    stream
        .write_all(&body_bytes)
        .await
        .map_err(|err| format!("write MCP HTTP body failed: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("flush MCP HTTP request failed: {err}"))?;

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .map_err(|err| format!("read MCP HTTP response failed: {err}"))?;
    let header_end = response_bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "MCP HTTP response missing header terminator".to_owned())?;
    let header = std::str::from_utf8(&response_bytes[..header_end])
        .map_err(|err| format!("MCP HTTP response header was not UTF-8: {err}"))?;
    if !header.starts_with("HTTP/1.1 200") {
        return Err(format!("unexpected MCP HTTP response header: {header}"));
    }
    let response_body = std::str::from_utf8(&response_bytes[header_end + 4..])
        .map_err(|err| format!("MCP HTTP response body was not UTF-8: {err}"))?;
    let json_str = response_body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .ok_or_else(|| format!("no SSE data line in MCP response body: {response_body}"))?;
    serde_json::from_str(json_str)
        .map_err(|err| format!("failed to parse MCP SSE JSON response: {err}"))
}

fn parse_http_url(url: &str) -> Result<(&str, &str), String> {
    let without_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("expected http:// URL, got {url}"))?;
    let slash = without_scheme
        .find('/')
        .ok_or_else(|| format!("expected path in URL {url}"))?;
    Ok((&without_scheme[..slash], &without_scheme[slash..]))
}

async fn emit_exit_plan_mode_request(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    stream_end_before_request: bool,
) -> Option<String> {
    if events_tx
        .send(ChatEvent::TypingStatusChanged(true))
        .is_err()
    {
        return None;
    }
    if stream_end_before_request {
        let message_id = Some(Uuid::new_v4().to_string());
        if events_tx
            .send(ChatEvent::StreamStart(StreamStartData {
                message_id: message_id.clone(),
                agent: "mock".to_owned(),
                model: Some(MOCK_MODEL.to_owned()),
            }))
            .is_err()
        {
            return None;
        }
        if events_tx
            .send(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: message_id.clone(),
                text: "mock ExitPlanMode waiting for approval".to_owned(),
            }))
            .is_err()
        {
            return None;
        }
        if events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    message_id: message_id.map(protocol::ChatMessageId),
                    timestamp: now_ms(),
                    sender: MessageSender::Assistant {
                        agent: "mock".to_owned(),
                    },
                    content: "mock ExitPlanMode waiting for approval".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: Some(ModelInfo {
                        model: MOCK_MODEL.to_owned(),
                    }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
            .is_err()
        {
            return None;
        }
    } else if events_tx
        .send(ChatEvent::MessageAdded(ChatMessage {
            message_id: Some(protocol::ChatMessageId(Uuid::new_v4().to_string())),
            timestamp: now_ms(),
            sender: MessageSender::Assistant {
                agent: "mock".to_owned(),
            },
            content: "mock ExitPlanMode waiting for approval".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: Some(ModelInfo {
                model: MOCK_MODEL.to_owned(),
            }),
            token_usage: None,
            context_breakdown: None,
            images: None,
        }))
        .is_err()
    {
        return None;
    }
    let tool_call_id = MOCK_EXIT_PLAN_MODE_TOOL_CALL_ID.to_owned();
    if events_tx
        .send(ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: tool_call_id.clone(),
            tool_name: "ExitPlanMode".to_owned(),
            tool_type: ToolRequestType::ExitPlanMode {
                plan: Some(MOCK_EXIT_PLAN_MODE_PLAN.to_owned()),
                plan_path: Some(MOCK_EXIT_PLAN_MODE_PLAN_PATH.to_owned()),
            },
        }))
        .is_err()
    {
        return None;
    }
    if events_tx
        .send(ChatEvent::TypingStatusChanged(false))
        .is_err()
    {
        return None;
    }
    Some(tool_call_id)
}

async fn handle_exit_plan_mode_tool_response(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id: &SessionId,
    pending_exit_plan_mode: &mut Option<PendingExitPlanMode>,
    tool_response: protocol::SendMessageToolResponse,
) {
    let protocol::SendMessageToolResponse::ExitPlanMode {
        tool_call_id,
        decision,
        feedback,
    } = tool_response;
    let Some(pending) = pending_exit_plan_mode.as_ref() else {
        emit_mock_error(
            events_tx,
            "No matching pending tool request is waiting for that response.",
        );
        return;
    };
    if pending.tool_call_id != tool_call_id {
        emit_mock_error(
            events_tx,
            &format!(
                "ExitPlanMode response targeted stale tool_call_id {tool_call_id}; pending tool_call_id is {}.",
                pending.tool_call_id
            ),
        );
        return;
    }

    let pending = pending_exit_plan_mode
        .take()
        .expect("pending ExitPlanMode response disappeared after validation");
    if !pending.delay_before_completion.is_zero() {
        sleep(pending.delay_before_completion).await;
    }
    emit_exit_plan_mode_completion(
        events_tx,
        session_id,
        &tool_call_id,
        decision,
        feedback,
        pending.delay_after_completion,
    )
    .await;
}

async fn emit_exit_plan_mode_completion(
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id: &SessionId,
    tool_call_id: &str,
    decision: protocol::ExitPlanModeDecision,
    feedback: Option<String>,
    delay_after_completion: Duration,
) -> bool {
    let approved = decision == protocol::ExitPlanModeDecision::Approve;
    let decision_label = if approved { "approved" } else { "rejected" };
    let message = if approved {
        "mock ExitPlanMode approved".to_owned()
    } else {
        format!(
            "mock ExitPlanMode rejected: {}",
            feedback.as_deref().unwrap_or("Plan rejected by user.")
        )
    };

    if events_tx
        .send(ChatEvent::ToolExecutionCompleted(
            ToolExecutionCompletedData {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({
                        "decision": decision_label,
                        "feedback": feedback,
                    }),
                },
                success: true,
                error: None,
                normalization_failure: None,
            },
        ))
        .is_err()
    {
        return false;
    }
    if !delay_after_completion.is_zero() {
        sleep(delay_after_completion).await;
    }
    emit_turn(events_tx, session_id, &message, false).await
}

fn emit_mock_orchestration(events_tx: &mpsc::UnboundedSender<ChatEvent>) {
    let _ = events_tx.send(ChatEvent::Orchestration(OrchestrationEvent {
        agent_id: OrchestrationId("mock-root".to_owned()),
        agent_type: OrchestrationAgentType("swarm".to_owned()),
        payload: OrchestrationPayload::AgentStarted {
            parent_agent_id: None,
            task_preview: "mock orchestration".to_owned(),
            origin: OrchestrationAgentOrigin::Root,
            depth: 1,
            interactive: true,
            model: None,
        },
    }));
    let _ = events_tx.send(ChatEvent::TypingStatusChanged(false));
}

fn emit_mock_error(events_tx: &mpsc::UnboundedSender<ChatEvent>, message: &str) {
    let _ = events_tx.send(ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender: MessageSender::Error,
        content: message.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }));
}

fn emit_mock_tool_failure_without_idle(events_tx: &mpsc::UnboundedSender<ChatEvent>) {
    let message_id = Some(Uuid::new_v4().to_string());
    let _ = events_tx.send(ChatEvent::TypingStatusChanged(true));
    let _ = events_tx.send(ChatEvent::StreamStart(StreamStartData {
        message_id: message_id.clone(),
        agent: "mock".to_owned(),
        model: Some(MOCK_MODEL.to_owned()),
    }));
    let _ = events_tx.send(ChatEvent::ToolRequest(ToolRequest {
        tool_call_id: MOCK_FAILED_TOOL_CALL_ID.to_owned(),
        tool_name: "Bash".to_owned(),
        tool_type: ToolRequestType::RunCommand {
            command: "mock interrupted command".to_owned(),
            working_directory: "/tmp/test".to_owned(),
        },
    }));
    let _ = events_tx.send(ChatEvent::ToolExecutionCompleted(
        ToolExecutionCompletedData {
            tool_call_id: MOCK_FAILED_TOOL_CALL_ID.to_owned(),
            tool_name: "Bash".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "Tool execution was interrupted".to_owned(),
                detailed_message: "Claude history did not contain a tool_result before the conversation advanced; treating the tool as interrupted.".to_owned(),
            },
            success: false,
            error: Some(
                "Claude history did not contain a tool_result before the conversation advanced; treating the tool as interrupted."
                    .to_owned(),
            ),
            normalization_failure: None,
        },
    ));
}

async fn maybe_spawn_native_child(
    prompt: &str,
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    active_subagents: &mut Vec<SubAgentHandle>,
) {
    let drop_handle = prompt.contains(SPAWN_NATIVE_CHILD_AND_DROP_SENTINEL);
    if !drop_handle && !prompt.contains(SPAWN_NATIVE_CHILD_SENTINEL) {
        return;
    }

    // Parent session registration happens on a separate host task. Give it a
    // moment so the backend-native child can inherit the persisted parent id.
    sleep(Duration::from_millis(50)).await;

    let emitter = wait_for_subagent_emitter(subagent_emitter_rx).await;
    let clean_prompt = prompt
        .replace(SPAWN_NATIVE_CHILD_AND_DROP_SENTINEL, "")
        .replace(SPAWN_NATIVE_CHILD_SENTINEL, "");
    let clean_prompt = clean_prompt.trim();
    let child_prompt = if clean_prompt.is_empty() {
        "native child task"
    } else {
        clean_prompt
    };
    let tool_use_id = format!("mock-tool-use-{}", Uuid::new_v4());

    let handle = match emitter
        .on_subagent_spawned(
            tool_use_id,
            "mock-native-child".to_owned(),
            child_prompt.to_owned(),
            "mock".to_owned(),
            Some(SessionId(Uuid::new_v4().to_string())),
        )
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            tracing::error!(%error, "mock native child relay registration failed");
            return;
        }
    };

    emit_native_child_turn(&handle.event_tx, child_prompt);
    if drop_handle {
        // Dropping the handle closes event_tx, so the relay actor sees
        // events.recv() == None and must park instead of exiting.
        drop(handle);
    } else {
        active_subagents.push(handle);
    }
}

async fn wait_for_subagent_emitter(
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
) -> Arc<dyn SubAgentEmitter> {
    loop {
        if let Some(emitter) = subagent_emitter_rx.borrow().clone() {
            return emitter;
        }
        subagent_emitter_rx
            .changed()
            .await
            .expect("mock sub-agent emitter sender dropped before registration");
    }
}

fn emit_native_child_turn(event_tx: &mpsc::UnboundedSender<ChatEvent>, prompt: &str) {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = format!("mock native child response to: {prompt}");

    let _ = event_tx.send(ChatEvent::TypingStatusChanged(true));
    let _ = event_tx.send(ChatEvent::StreamStart(StreamStartData {
        message_id: message_id.clone(),
        agent: "mock-native-child".to_owned(),
        model: Some(MOCK_MODEL.to_owned()),
    }));
    let _ = event_tx.send(ChatEvent::StreamDelta(StreamTextDeltaData {
        message_id: message_id.clone(),
        text: response_text.clone(),
    }));
    let _ = event_tx.send(ChatEvent::StreamEnd(StreamEndData {
        message: ChatMessage {
            message_id: message_id.map(protocol::ChatMessageId),
            timestamp: now_ms(),
            sender: MessageSender::Assistant {
                agent: "mock-native-child".to_owned(),
            },
            content: response_text,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: Some(ModelInfo {
                model: MOCK_MODEL.to_owned(),
            }),
            token_usage: Some(MessageTokenUsage::request_and_turn_known(
                TokenUsage {
                    input_tokens: 250,
                    output_tokens: 80,
                    total_tokens: 330,
                    cached_prompt_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
                TokenUsage {
                    input_tokens: 250,
                    output_tokens: 80,
                    total_tokens: 330,
                    cached_prompt_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
            )),
            context_breakdown: None,
            images: None,
        },
    }));
    let _ = event_tx.send(ChatEvent::TypingStatusChanged(false));
}

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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::customization::ResolvedSpawnConfig;

    #[tokio::test]
    async fn mock_backend_records_read_only_access_mode() {
        let (backend, _events) = MockBackend::spawn(
            vec!["/tmp".to_string()],
            BackendSpawnConfig {
                resolved_spawn_config: ResolvedSpawnConfig {
                    access_mode: BackendAccessMode::ReadOnly,
                    ..Default::default()
                },
                ..Default::default()
            },
            protocol::SendMessagePayload {
                message: "hello".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn mock backend");

        let store = session_store()
            .lock()
            .expect("mock backend session store mutex poisoned");
        let record = store
            .get(&backend.session_id.0)
            .expect("mock session record");
        assert_eq!(record.access_mode, BackendAccessMode::ReadOnly);
    }

    #[tokio::test]
    async fn empty_agent_control_output_uses_one_stream_identity() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        assert!(
            emit_turn(
                &events_tx,
                &SessionId("mock-empty-output-session".to_owned()),
                EMPTY_AGENT_CONTROL_OUTPUT_SENTINEL,
                false,
            )
            .await
        );

        let events = std::iter::from_fn(|| events_rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], ChatEvent::TypingStatusChanged(true)));
        let start_id = match &events[1] {
            ChatEvent::StreamStart(start) => start
                .message_id
                .as_ref()
                .filter(|message_id| !message_id.is_empty())
                .expect("empty-output StreamStart identity"),
            event => panic!("expected StreamStart, got {event:?}"),
        };
        let completed = match &events[2] {
            ChatEvent::StreamEnd(end) => &end.message,
            event => panic!("expected StreamEnd, got {event:?}"),
        };
        assert_eq!(
            completed
                .message_id
                .as_ref()
                .expect("empty-output StreamEnd identity")
                .0
                .as_str(),
            start_id.as_str()
        );
        assert!(completed.content.is_empty());
        assert_eq!(
            completed
                .reasoning
                .as_ref()
                .map(|reasoning| reasoning.text.as_str()),
            Some("hidden reasoning")
        );
        assert!(matches!(&events[3], ChatEvent::TypingStatusChanged(false)));
        assert!(events.iter().all(|event| !matches!(
            event,
            ChatEvent::MessageAdded(ChatMessage {
                sender: MessageSender::Error,
                ..
            })
        )));
    }
}
