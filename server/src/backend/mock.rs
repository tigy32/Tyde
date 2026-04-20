use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, ModelInfo,
    OperationCancelledData, SessionId, StreamEndData, StreamStartData, StreamTextDeltaData,
    TokenUsage, ToolPolicy,
};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use super::empty_session_settings_schema;
use super::{Backend, BackendSession, BackendSpawnConfig, EventStream, StartupMcpTransport};
use crate::sub_agent::{SubAgentEmitter, SubAgentHandle};

const INPUT_BUFFER: usize = 64;
const EVENT_BUFFER: usize = 256;
const MOCK_MODEL: &str = "mock";
const FORCE_SPAWN_FAILURE_SENTINEL: &str = "__mock_fail_spawn__";
const SPAWN_NATIVE_CHILD_SENTINEL: &str = "__mock_spawn_native_child__";
const MOCK_CANCEL_TURN_SENTINEL: &str = "__mock_cancel__";
const MOCK_COMPACT_SENTINEL: &str = "/compact";
/// Causes `emit_turn` to sleep 2 s before emitting `TypingStatusChanged(false)`.
/// This gives tests a reliable window to send queued messages while the agent
/// is still in-turn, without relying on wall-clock races.  The long window also
/// gives replay tests enough time to connect a second client and verify state.
pub(crate) const MOCK_SLOW_TURN_SENTINEL: &str = "__mock_slow__";
/// Causes the mock backend task to emit `TypingStatusChanged(true)`, sleep 300 ms,
/// then exit without completing the turn.  The events channel closes when the
/// task exits, which drives the agent actor into `enter_terminal_failure`.
pub(crate) const MOCK_DIE_AFTER_BUSY_SENTINEL: &str = "__mock_die_after_busy__";
/// Sleep for __mock_slow__ turns — long enough for replay tests to connect a
/// second client and see the queued-message snapshot before the turn ends.
const MOCK_SLOW_SLEEP_MS: u64 = 2_000;
/// Sleep for __mock_die_after_busy__ — just enough for tests to queue messages.
const MOCK_DIE_SLEEP_MS: u64 = 300;

#[derive(Debug, Clone)]
struct MockSessionRecord {
    workspace_roots: Vec<String>,
    prompts: Vec<String>,
    startup_mcp_servers: Vec<String>,
    instructions: Option<String>,
    steering_body: String,
    skills: Vec<String>,
    tool_policy: ToolPolicy,
    created_at_ms: u64,
    updated_at_ms: u64,
}

fn session_store() -> &'static Mutex<HashMap<String, MockSessionRecord>> {
    static STORE: OnceLock<Mutex<HashMap<String, MockSessionRecord>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct MockBackend {
    command_tx: mpsc::Sender<MockCommand>,
    session_id: SessionId,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
}

enum MockCommand {
    Input(AgentInput),
    Interrupt,
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
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
        }

        let (command_tx, mut command_rx) = mpsc::channel::<MockCommand>(INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let session_id_for_task = session_id.clone();

        tokio::spawn(async move {
            if initial_message.contains(MOCK_DIE_AFTER_BUSY_SENTINEL) {
                // Send TypingStatusChanged(true) so the actor sets in_turn=true,
                // then sleep to give tests time to queue messages, then return so
                // that events_tx is dropped and the actor detects termination.
                let _ = events_tx.send(ChatEvent::TypingStatusChanged(true)).await;
                sleep(Duration::from_millis(MOCK_DIE_SLEEP_MS)).await;
                return;
            }
            let mut active_subagents = Vec::new();
            record_prompt(&session_id_for_task, &initial_message);
            if !emit_turn(&events_tx, &session_id_for_task, &initial_message).await {
                return;
            }
            maybe_spawn_native_child(
                &initial_message,
                &mut subagent_emitter_rx,
                &mut active_subagents,
            )
            .await;

            while let Some(command) = command_rx.recv().await {
                match command {
                    MockCommand::Input(AgentInput::SendMessage(payload)) => {
                        record_prompt(&session_id_for_task, &payload.message);
                        if !emit_turn(&events_tx, &session_id_for_task, &payload.message).await {
                            return;
                        }
                        maybe_spawn_native_child(
                            &payload.message,
                            &mut subagent_emitter_rx,
                            &mut active_subagents,
                        )
                        .await;
                    }
                    MockCommand::Input(AgentInput::UpdateSessionSettings(_)) => {}
                    MockCommand::Input(AgentInput::EditQueuedMessage(_))
                    | MockCommand::Input(AgentInput::CancelQueuedMessage(_))
                    | MockCommand::Input(AgentInput::SendQueuedMessageNow(_)) => {
                        panic!(
                            "queued-message inputs must be handled by the agent actor before reaching the backend"
                        );
                    }
                    MockCommand::Interrupt => break,
                }
            }

            drop(active_subagents);
        });

        Ok((
            Self {
                command_tx,
                session_id,
                subagent_emitter_tx,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let startup_mcp_servers = config
            .startup_mcp_servers
            .iter()
            .map(|server| match &server.transport {
                StartupMcpTransport::Http { .. } => format!("{}(http)", server.name),
                StartupMcpTransport::Stdio { .. } => format!("{}(stdio)", server.name),
            })
            .collect::<Vec<_>>();
        let resolved_spawn_config = config.resolved_spawn_config.clone();
        {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            let Some(record) = store.get_mut(&session_id.0) else {
                return Err(format!("unknown mock session {}", session_id.0));
            };
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
            record.updated_at_ms = now_ms();
        }

        let (command_tx, mut command_rx) = mpsc::channel::<MockCommand>(INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let session_id_for_task = session_id.clone();

        tokio::spawn(async move {
            let mut active_subagents = Vec::new();
            while let Some(command) = command_rx.recv().await {
                match command {
                    MockCommand::Input(AgentInput::SendMessage(payload)) => {
                        record_prompt(&session_id_for_task, &payload.message);
                        if !emit_turn(&events_tx, &session_id_for_task, &payload.message).await {
                            return;
                        }
                        maybe_spawn_native_child(
                            &payload.message,
                            &mut subagent_emitter_rx,
                            &mut active_subagents,
                        )
                        .await;
                    }
                    MockCommand::Input(AgentInput::UpdateSessionSettings(_)) => {}
                    MockCommand::Input(AgentInput::EditQueuedMessage(_))
                    | MockCommand::Input(AgentInput::CancelQueuedMessage(_))
                    | MockCommand::Input(AgentInput::SendQueuedMessageNow(_)) => {
                        panic!(
                            "queued-message inputs must be handled by the agent actor before reaching the backend"
                        );
                    }
                    MockCommand::Interrupt => break,
                }
            }

            drop(active_subagents);
        });

        Ok((
            Self {
                command_tx,
                session_id,
                subagent_emitter_tx,
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
        sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(sessions)
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.command_tx
            .send(MockCommand::Input(input))
            .await
            .is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.command_tx.send(MockCommand::Interrupt).await.is_ok()
    }

    async fn shutdown(self) {
        drop(self);
    }
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

async fn emit_turn(
    events_tx: &mpsc::Sender<ChatEvent>,
    session_id: &SessionId,
    user_message: &str,
) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = format!(
        "{}mock backend response to: {user_message}",
        startup_mcp_response_prefix(session_id)
    );

    if events_tx
        .send(ChatEvent::TypingStatusChanged(true))
        .await
        .is_err()
    {
        return false;
    }

    if user_message.contains(MOCK_CANCEL_TURN_SENTINEL) {
        if events_tx
            .send(ChatEvent::OperationCancelled(OperationCancelledData {
                message: format!("mock backend cancelled: {user_message}"),
            }))
            .await
            .is_err()
        {
            return false;
        }
        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .await
            .is_ok();
    }

    if user_message.trim() == MOCK_COMPACT_SENTINEL {
        if events_tx
            .send(ChatEvent::MessageAdded(ChatMessage {
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
            .await
            .is_err()
        {
            return false;
        }

        if events_tx
            .send(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
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
            .await
            .is_err()
        {
            return false;
        }

        return events_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .await
            .is_ok();
    }

    if events_tx
        .send(ChatEvent::StreamStart(StreamStartData {
            message_id: message_id.clone(),
            agent: "mock".to_owned(),
            model: Some(MOCK_MODEL.to_owned()),
        }))
        .await
        .is_err()
    {
        return false;
    }

    if events_tx
        .send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: message_id.clone(),
            text: response_text.clone(),
        }))
        .await
        .is_err()
    {
        return false;
    }

    let message = ChatMessage {
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
        token_usage: Some(TokenUsage {
            input_tokens: 1250,
            output_tokens: 340,
            total_tokens: 1590,
            cached_prompt_tokens: Some(800),
            cache_creation_input_tokens: Some(50),
            reasoning_tokens: Some(120),
        }),
        context_breakdown: None,
        images: None,
    };

    if events_tx
        .send(ChatEvent::StreamEnd(StreamEndData { message }))
        .await
        .is_err()
    {
        return false;
    }

    if user_message.contains(MOCK_SLOW_TURN_SENTINEL) {
        // Yield here so the Tokio scheduler can run client tasks and allow tests
        // to send queued messages before the turn officially ends.
        sleep(Duration::from_millis(MOCK_SLOW_SLEEP_MS)).await;
    }

    events_tx
        .send(ChatEvent::TypingStatusChanged(false))
        .await
        .is_ok()
}

async fn maybe_spawn_native_child(
    prompt: &str,
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    active_subagents: &mut Vec<SubAgentHandle>,
) {
    if !prompt.contains(SPAWN_NATIVE_CHILD_SENTINEL) {
        return;
    }

    // Parent session registration happens on a separate host task. Give it a
    // moment so the backend-native child can inherit the persisted parent id.
    sleep(Duration::from_millis(50)).await;

    let emitter = wait_for_subagent_emitter(subagent_emitter_rx).await;
    let clean_prompt = prompt.replace(SPAWN_NATIVE_CHILD_SENTINEL, "");
    let clean_prompt = clean_prompt.trim();
    let child_prompt = if clean_prompt.is_empty() {
        "native child task"
    } else {
        clean_prompt
    };
    let tool_use_id = format!("mock-tool-use-{}", Uuid::new_v4());

    let handle = emitter
        .on_subagent_spawned(
            tool_use_id,
            "mock-native-child".to_owned(),
            child_prompt.to_owned(),
            "mock".to_owned(),
            Some(SessionId(Uuid::new_v4().to_string())),
        )
        .await;

    emit_native_child_turn(&handle.event_tx, child_prompt);
    active_subagents.push(handle);
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
            token_usage: Some(TokenUsage {
                input_tokens: 250,
                output_tokens: 80,
                total_tokens: 330,
                cached_prompt_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                reasoning_tokens: Some(0),
            }),
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
