use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentOrigin, AgentRenamedPayload,
    AgentStartPayload, BackendKind, ChatEvent, Envelope, FrameKind, MessageSender,
    QueuedMessageEntry, QueuedMessageId, QueuedMessagesPayload, SendMessagePayload, SessionId,
    SessionSettingsPayload, SessionSettingsValues, SpawnCostHint,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::backend::claude::ClaudeBackend;
use crate::backend::codex::CodexBackend;
use crate::backend::gemini::GeminiBackend;
use crate::backend::kiro::KiroBackend;
use crate::backend::mock::MockBackend;
use crate::backend::tycode::TycodeBackend;
use crate::backend::{
    Backend, BackendSession, BackendSpawnConfig, EventStream, StartupMcpServer,
    apply_session_settings_update, resolve_backend_session_settings,
    validate_session_settings_values,
};
use crate::host::{
    ChildCompletionNotice, ChildCompletionOutcome, HostChildCompletionNoticeTx, HostSubAgentEmitter,
};
use crate::store::session::SessionStore;
use crate::stream::Stream;
use crate::sub_agent::HostSubAgentSpawnTx;

pub(crate) mod customization;
pub(crate) mod registry;

use self::registry::{InitialAgentAlias, InitialAgentAliasPersistence, ResolvedSpawnRequest};

const COMMAND_BUFFER: usize = 64;
const IMAGE_ONLY_AGENT_NAME: &str = "Image Review Task";
const BACKEND_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

type BackendHandle = Box<dyn BackendSender>;
type BackendSpawnResult = Result<(BackendHandle, EventStream, SessionId), String>;
type BackendResumeResult = Result<(BackendHandle, EventStream), String>;
type BackendFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;

struct TerminalFailureContext<'a> {
    accepting_input: &'a Arc<AtomicBool>,
    status_handle: &'a registry::AgentStatusHandle,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    subscribers: &'a mut Vec<Stream>,
    queue: &'a mut VecDeque<QueuedMessageEntry>,
    start: &'a AgentStartPayload,
    child_completion_tx: &'a HostChildCompletionNoticeTx,
}

struct AgentNameChangeContext<'a> {
    session_store: &'a Arc<Mutex<SessionStore>>,
    session_id: Option<&'a SessionId>,
    pending_alias: &'a mut Option<InitialAgentAlias>,
    current_start: &'a mut AgentStartPayload,
    event_log: &'a mut [Envelope],
    subscribers: &'a mut Vec<Stream>,
}

enum AgentCommand {
    SendInput(AgentInput),
    EnqueueAutoFollowUp {
        message: String,
    },
    SetName {
        name: String,
        persistence: AgentNamePersistence,
        reply: oneshot::Sender<bool>,
    },
    Snapshot {
        reply: oneshot::Sender<AgentStartPayload>,
    },
    Interrupt,
    Close {
        reply: oneshot::Sender<()>,
    },
    Attach(Stream),
}

#[derive(Clone, Copy)]
enum AgentNamePersistence {
    User,
    GeneratedIfNoUserAlias,
}

#[derive(Clone)]
pub(crate) struct AgentHandle {
    tx: mpsc::Sender<AgentCommand>,
    accepting_input: Arc<AtomicBool>,
}

impl AgentHandle {
    pub async fn send_input(&self, input: AgentInput) -> bool {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return false;
        }
        self.tx.send(AgentCommand::SendInput(input)).await.is_ok()
    }

    pub async fn set_name(&self, name: String) -> Option<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::SetName {
                name,
                persistence: AgentNamePersistence::User,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn set_generated_name(&self, name: String) -> Option<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::SetName {
                name,
                persistence: AgentNamePersistence::GeneratedIfNoUserAlias,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn snapshot(&self) -> Option<AgentStartPayload> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Snapshot { reply: reply_tx })
            .await
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn interrupt(&self) -> bool {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return false;
        }
        self.tx.send(AgentCommand::Interrupt).await.is_ok()
    }

    pub async fn close(&self) -> bool {
        self.accepting_input.store(false, Ordering::SeqCst);
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Close { reply: reply_tx })
            .await
            .is_err()
        {
            return false;
        }
        reply_rx.await.is_ok()
    }

    pub async fn attach(&self, stream: Stream) -> bool {
        self.tx.send(AgentCommand::Attach(stream)).await.is_ok()
    }

    pub async fn enqueue_auto_follow_up(&self, message: String) -> bool {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return false;
        }
        self.tx
            .send(AgentCommand::EnqueueAutoFollowUp { message })
            .await
            .is_ok()
    }
}

#[derive(Default)]
struct TurnLocalCompletionState {
    completed_message: Option<String>,
}

impl TurnLocalCompletionState {
    fn reset(&mut self) {
        self.completed_message = None;
    }
}

enum ActorLifecycle {
    Running,
    Closing,
}

pub(crate) struct GenerateAgentNameRequest {
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub prompt: String,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub use_mock_backend: bool,
}

pub(crate) async fn generate_agent_name(
    request: GenerateAgentNameRequest,
) -> Result<String, String> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Ok(IMAGE_ONLY_AGENT_NAME.to_string());
    }

    if request.use_mock_backend {
        return generate_mock_name(prompt);
    }

    let name_prompt = build_name_generation_prompt(prompt);
    let logged_name_prompt = name_prompt.clone();
    let startup_mcp_server_names = request
        .startup_mcp_servers
        .iter()
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    let spawn_config = BackendSpawnConfig {
        cost_hint: Some(SpawnCostHint::Low),
        custom_agent_id: None,
        startup_mcp_servers: request.startup_mcp_servers,
        session_settings: None,
        resolved_spawn_config: Default::default(),
    };
    let initial_input = SendMessagePayload {
        message: name_prompt,
        images: None,
    };
    let name_agent_id = AgentId(Uuid::new_v4().to_string());
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    let (_backend, mut events, _session_id) = match spawn_backend(
        &name_agent_id,
        request.backend_kind,
        request.workspace_roots,
        spawn_config,
        initial_input,
        host_sub_agent_spawn_tx,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Err(format!(
                "agent name generator failed to start for backend {:?}: {}",
                request.backend_kind, err
            ));
        }
    };

    let mut streamed_text = String::new();
    let mut stream_delta_count = 0usize;
    let mut chat_event_count = 0usize;
    while let Some(event) = events.recv().await {
        chat_event_count += 1;
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                tracing::warn!(
                    backend_kind = ?request.backend_kind,
                    cost_hint = ?SpawnCostHint::Low,
                    prompt = %prompt,
                    name_prompt = %logged_name_prompt,
                    chat_event_count,
                    stream_delta_count,
                    startup_mcp_servers = ?startup_mcp_server_names,
                    error_message = %message.content,
                    "agent name generator received a backend error"
                );
                return Err(message.content);
            }
            ChatEvent::StreamDelta(delta) => {
                stream_delta_count += 1;
                streamed_text.push_str(&delta.text);
            }
            ChatEvent::StreamEnd(data) => {
                let final_content = data.message.content;
                let streamed_text_len = streamed_text.len();
                let candidate = if final_content.trim().is_empty() {
                    streamed_text
                } else {
                    final_content.clone()
                };
                if candidate.trim().is_empty() {
                    tracing::warn!(
                        backend_kind = ?request.backend_kind,
                        cost_hint = ?SpawnCostHint::Low,
                        prompt = %prompt,
                        name_prompt = %logged_name_prompt,
                        chat_event_count,
                        stream_delta_count,
                        final_content_len = final_content.len(),
                        streamed_text_len,
                        startup_mcp_servers = ?startup_mcp_server_names,
                        "agent name generator received an empty assistant response"
                    );
                }
                return Ok(name_generation_fallback(prompt, &candidate));
            }
            _ => {}
        }
    }

    tracing::warn!(
        backend_kind = ?request.backend_kind,
        cost_hint = ?SpawnCostHint::Low,
        prompt = %prompt,
        name_prompt = %logged_name_prompt,
        chat_event_count,
        stream_delta_count,
        startup_mcp_servers = ?startup_mcp_server_names,
        "agent name generator ended before producing a final response"
    );
    Err("agent name generator ended before producing a final response".to_string())
}

/// Type-erased backend handle. The actor loop only needs `send()` — this lets
/// us dispatch to any concrete `Backend` at spawn time and forget the type.
trait BackendSender: Send + 'static {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

impl<B: Backend> BackendSender for B {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::send(self, input))
    }

    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::interrupt(self))
    }

    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            Backend::shutdown(*self).await;
        })
    }
}

/// Spawn the correct backend based on `backend_kind`.
/// If the backend already knows its native resumable session ID, return it.
async fn spawn_backend(
    agent_id: &AgentId,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendSpawnResult {
    match backend_kind {
        BackendKind::Tycode => {
            let (b, events) = TycodeBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Kiro => {
            let (b, events) = KiroBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Claude => {
            let emitter = Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots.clone(),
            ));
            let (b, events) = ClaudeBackend::spawn_with_subagent_emitter(
                workspace_roots,
                config,
                initial_input,
                emitter,
            )
            .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Codex => {
            let (b, events) =
                CodexBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Gemini => {
            let (b, events) = GeminiBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
    }
}

async fn resume_backend(
    agent_id: &AgentId,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    session_id: SessionId,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendResumeResult {
    let (backend, events): (BackendHandle, EventStream) = match backend_kind {
        BackendKind::Tycode => {
            let (b, events) = TycodeBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Kiro => {
            let (b, events) = KiroBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Claude => {
            let (b, events) =
                ClaudeBackend::resume(workspace_roots.clone(), config, session_id.clone()).await?;
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) =
                CodexBackend::resume(workspace_roots.clone(), config, session_id.clone()).await?;
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            (Box::new(b), events)
        }
        BackendKind::Gemini => {
            let (b, events) = GeminiBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
    };
    Ok((backend, events))
}

fn spawn_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendFuture<BackendSpawnResult> {
    Box::pin(async move {
        let (b, events) =
            MockBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
        let sid = Backend::session_id(&b);
        b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
            host_sub_agent_spawn_tx,
            agent_id,
            workspace_roots,
        )))
        .await;
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

fn resume_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    session_id: SessionId,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendFuture<BackendResumeResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::resume(
            workspace_roots.clone(),
            BackendSpawnConfig::default(),
            session_id.clone(),
        )
        .await?;
        b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
            host_sub_agent_spawn_tx,
            agent_id,
            workspace_roots,
        )))
        .await;
        Ok((Box::new(b) as BackendHandle, events))
    })
}

pub(crate) fn spawn_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    request: ResolvedSpawnRequest,
    session_store: Arc<Mutex<SessionStore>>,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    child_completion_tx: HostChildCompletionNoticeTx,
    status_handle: registry::AgentStatusHandle,
) -> (AgentHandle, oneshot::Receiver<Result<SessionId, String>>) {
    let (tx, mut rx) = mpsc::channel::<AgentCommand>(COMMAND_BUFFER);
    let accepting_input = Arc::new(AtomicBool::new(false));
    let accepting_input_task = Arc::clone(&accepting_input);
    let (startup_tx, startup_rx) = oneshot::channel();

    tokio::spawn(async move {
        let ResolvedSpawnRequest {
            parent_session_id,
            backend_kind,
            workspace_roots,
            initial_input,
            cost_hint,
            session_settings,
            session_settings_schema,
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id,
            startup_warning,
            startup_failure,
            initial_alias,
            use_mock_backend,
            ..
        } = request;
        let mut current_start = start.clone();
        let spawn_config = BackendSpawnConfig {
            cost_hint,
            custom_agent_id: current_start.custom_agent_id.clone(),
            startup_mcp_servers,
            session_settings,
            resolved_spawn_config: resolved_spawn_config.clone(),
        };
        let initial_cost_hint = spawn_config.cost_hint;
        let initial_session_settings = spawn_config.session_settings.clone();
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut current_session_id = resume_session_id.clone();
        let mut pending_alias = initial_alias;
        let session_schema = session_settings_schema;
        let mut current_session_settings = resolve_backend_session_settings(
            backend_kind,
            &BackendSpawnConfig {
                cost_hint: initial_cost_hint,
                custom_agent_id: current_start.custom_agent_id.clone(),
                startup_mcp_servers: Vec::new(),
                session_settings: initial_session_settings,
                resolved_spawn_config,
            },
        );
        let mut queue = VecDeque::new();
        let mut turn_completion = TurnLocalCompletionState::default();
        let is_new_spawn = resume_session_id.is_none();

        let startup_result: Result<
            (
                BackendHandle,
                EventStream,
                SessionId,
                Option<SendMessagePayload>,
            ),
            String,
        > = if let Some(err) = startup_failure {
            Err(err)
        } else {
            match resume_session_id {
                Some(session_id) => {
                    let resumed = if use_mock_backend {
                        resume_mock(
                            agent_id.clone(),
                            workspace_roots.clone(),
                            session_id.clone(),
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    } else {
                        resume_backend(
                            &agent_id,
                            backend_kind,
                            workspace_roots.clone(),
                            spawn_config.clone(),
                            session_id.clone(),
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    };
                    resumed.map(|(backend, events)| (backend, events, session_id, initial_input))
                }
                None => {
                    let first_input = initial_input.expect("new spawn requires initial_input");
                    let spawned = if use_mock_backend {
                        spawn_mock(
                            agent_id.clone(),
                            workspace_roots.clone(),
                            spawn_config,
                            first_input,
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    } else {
                        spawn_backend(
                            &agent_id,
                            backend_kind,
                            workspace_roots.clone(),
                            spawn_config,
                            first_input,
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    };
                    spawned.map(|(backend, events, session_id)| (backend, events, session_id, None))
                }
            }
        };

        let (backend, mut events, actor_session_id, initial_follow_up) = match startup_result {
            Ok(result) => result,
            Err(err) => {
                let _ = startup_tx.send(Err(err.clone()));
                let payload = AgentErrorPayload {
                    agent_id: current_start.agent_id.clone(),
                    code: AgentErrorCode::BackendFailed,
                    message: format!("failed to start agent backend: {err}"),
                    fatal: true,
                };
                append_event(
                    &canonical_stream,
                    &mut event_log,
                    &mut subscribers,
                    FrameKind::AgentStart,
                    &current_start,
                )
                .await;
                enter_terminal_failure(
                    TerminalFailureContext {
                        accepting_input: &accepting_input_task,
                        status_handle: &status_handle,
                        canonical_stream: &canonical_stream,
                        event_log: &mut event_log,
                        subscribers: &mut subscribers,
                        queue: &mut queue,
                        start: &current_start,
                        child_completion_tx: &child_completion_tx,
                    },
                    &payload,
                )
                .await;
                park_terminal_agent(
                    &session_store,
                    current_session_id.as_ref(),
                    &mut pending_alias,
                    &mut current_start,
                    &mut event_log,
                    &mut subscribers,
                    &mut rx,
                )
                .await;
                return;
            }
        };
        let mut backend = Some(backend);
        let mut in_turn = is_new_spawn;
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;
        current_session_id = Some(actor_session_id.clone());
        if let Err(err) = persist_agent_session(
            &session_store,
            &actor_session_id,
            parent_session_id,
            &current_start,
            &current_session_settings,
            &mut pending_alias,
        )
        .await
        {
            tracing::error!(
                agent_id = %current_start.agent_id,
                session_id = %actor_session_id,
                error = %err,
                "failed to persist agent session startup state"
            );
        }
        let _ = startup_tx.send(Ok(actor_session_id.clone()));
        accepting_input_task.store(true, Ordering::SeqCst);
        status_handle
            .update(|s| {
                s.started = true;
                s.last_error = None;
                s.activity_counter = s.activity_counter.saturating_add(1);
            })
            .await;
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &current_start,
        )
        .await;
        if let Some(warning) = startup_warning {
            append_event(
                &canonical_stream,
                &mut event_log,
                &mut subscribers,
                FrameKind::AgentError,
                &AgentErrorPayload {
                    agent_id: current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: warning,
                    fatal: false,
                },
            )
            .await;
        }
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::SessionSettings,
            &SessionSettingsPayload {
                values: current_session_settings.clone(),
            },
        )
        .await;
        update_queued_messages_snapshot(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            &queue,
        )
        .await;

        if let Some(input) = initial_follow_up.filter(|input| {
            !input.message.trim().is_empty()
                || input
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty())
        }) {
            in_turn = true;
            turn_completion.reset();
            if !backend
                .as_ref()
                .expect("backend must exist after successful startup")
                .send(AgentInput::SendMessage(input))
                .await
            {
                let payload = AgentErrorPayload {
                    agent_id: current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: "agent backend closed".to_owned(),
                    fatal: true,
                };
                enter_terminal_failure(
                    TerminalFailureContext {
                        accepting_input: &accepting_input_task,
                        status_handle: &status_handle,
                        canonical_stream: &canonical_stream,
                        event_log: &mut event_log,
                        subscribers: &mut subscribers,
                        queue: &mut queue,
                        start: &current_start,
                        child_completion_tx: &child_completion_tx,
                    },
                    &payload,
                )
                .await;
                park_terminal_agent(
                    &session_store,
                    current_session_id.as_ref(),
                    &mut pending_alias,
                    &mut current_start,
                    &mut event_log,
                    &mut subscribers,
                    &mut rx,
                )
                .await;
                return;
            }
        }

        loop {
            tokio::select! {
                maybe_event = events.recv() => {
                    let Some(event) = maybe_event else {
                        if matches!(lifecycle, ActorLifecycle::Closing) {
                            let reply = close_reply
                                .take()
                                .expect("close requested without pending close reply");
                            if let Some(backend) = backend.take() {
                                shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                            }
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
                        }
                        let payload = AgentErrorPayload {
                            agent_id: current_start.agent_id.clone(),
                            code: AgentErrorCode::BackendFailed,
                            message: "agent backend closed".to_owned(),
                            fatal: true,
                        };
                        enter_terminal_failure(
                            TerminalFailureContext {
                                accepting_input: &accepting_input_task,
                                status_handle: &status_handle,
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                subscribers: &mut subscribers,
                                queue: &mut queue,
                                start: &current_start,
                                child_completion_tx: &child_completion_tx,
                            },
                            &payload,
                        )
                        .await;
                        park_terminal_agent(
                            &session_store,
                            current_session_id.as_ref(),
                            &mut pending_alias,
                            &mut current_start,
                            &mut event_log,
                            &mut subscribers,
                            &mut rx,
                        )
                        .await;
                        return;
                    };
                    match &event {
                        ChatEvent::MessageAdded(message) => {
                            if matches!(message.sender, MessageSender::Error) {
                                let msg = message.content.clone();
                                status_handle.update(|s| {
                                    s.turn_completed = true;
                                    s.last_error = Some(msg);
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            } else {
                                status_handle.update(|s| {
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            }
                        }
                        ChatEvent::StreamStart(_) => {
                            active_stream_text.clear();
                            turn_completion.reset();
                            status_handle.update(|s| {
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
                        ChatEvent::StreamEnd(data) => {
                            active_stream_text.clear();
                            let msg = data.message.content.clone();
                            turn_completion.completed_message = Some(msg.clone());
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_message = Some(msg);
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::TypingStatusChanged(typing) => {
                            let typing = *typing;
                            if !typing {
                                in_turn = false;
                            } else {
                                turn_completion.reset();
                            }
                            status_handle.update(|s| {
                                s.is_thinking = typing;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(data) => {
                            let msg = data.message.clone();
                            turn_completion.reset();
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_message = Some(msg);
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        _ => {
                            status_handle.update(|s| {
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                    }
                    apply_runtime_session_updates(
                        &session_store,
                        current_session_id
                            .as_ref()
                            .expect("live agent must have session_id"),
                        &event,
                    )
                    .await;
                    append_event(
                        &canonical_stream,
                        &mut event_log,
                        &mut subscribers,
                        FrameKind::ChatEvent,
                        &event,
                    )
                    .await;

                    if let ChatEvent::OperationCancelled(data) = &event {
                        maybe_emit_child_completion_notice(
                            &child_completion_tx,
                            &current_start,
                            ChildCompletionOutcome::Cancelled,
                            data.message.clone(),
                        );
                    }

                    if matches!(event, ChatEvent::TypingStatusChanged(false))
                        && let Some(message_text) = turn_completion
                            .completed_message
                            .take()
                            .filter(|message| !message.trim().is_empty())
                    {
                        maybe_emit_child_completion_notice(
                            &child_completion_tx,
                            &current_start,
                            ChildCompletionOutcome::Completed,
                            message_text,
                        );
                    }

                    if matches!(event, ChatEvent::TypingStatusChanged(false))
                        && matches!(lifecycle, ActorLifecycle::Closing)
                    {
                        let reply = close_reply
                            .take()
                            .expect("close requested without pending close reply");
                        let backend = backend
                            .take()
                            .expect("backend must exist while closing a live actor");
                        shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                        finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                        return;
                    }

                    if matches!(event, ChatEvent::TypingStatusChanged(false))
                        && matches!(lifecycle, ActorLifecycle::Running)
                        && !queue.is_empty()
                    {
                        let queued = queue
                            .pop_front()
                            .expect("queue reported non-empty but pop_front returned None");
                        update_queued_messages_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &queue,
                        )
                        .await;
                        in_turn = true;
                        turn_completion.reset();
                        if !backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .send(AgentInput::SendMessage(queued_message_to_send_payload(queued)))
                            .await
                        {
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::Internal,
                                message: "agent backend closed".to_owned(),
                                fatal: true,
                            };
                            enter_terminal_failure(
                                TerminalFailureContext {
                                    accepting_input: &accepting_input_task,
                                    status_handle: &status_handle,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                    queue: &mut queue,
                                    start: &current_start,
                                    child_completion_tx: &child_completion_tx,
                                },
                                &payload,
                            )
                            .await;
                            park_terminal_agent(
                                &session_store,
                                current_session_id.as_ref(),
                                &mut pending_alias,
                                &mut current_start,
                                &mut event_log,
                                &mut subscribers,
                                &mut rx,
                            )
                            .await;
                            return;
                        }
                    }
                }
                maybe_command = rx.recv() => {
                    let Some(command) = maybe_command else {
                        break;
                    };
                    match command {
                        AgentCommand::SendInput(input) => {
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                continue;
                            }
                            match input {
                                AgentInput::SendMessage(msg) => {
                                    if in_turn {
                                        queue.push_back(QueuedMessageEntry {
                                            id: QueuedMessageId(Uuid::new_v4().to_string()),
                                            message: msg.message,
                                            images: msg.images.unwrap_or_default(),
                                        });
                                        update_queued_messages_snapshot(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &queue,
                                        )
                                        .await;
                                    } else {
                                        in_turn = true;
                                        turn_completion.reset();
                                        if !backend
                                            .as_ref()
                                            .expect("backend must exist while actor is running")
                                            .send(AgentInput::SendMessage(msg))
                                            .await
                                        {
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend closed".to_owned(),
                                                fatal: true,
                                            };
                                            enter_terminal_failure(
                                                TerminalFailureContext {
                                                    accepting_input: &accepting_input_task,
                                                    status_handle: &status_handle,
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    subscribers: &mut subscribers,
                                                    queue: &mut queue,
                                                    start: &current_start,
                                                    child_completion_tx: &child_completion_tx,
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
                                                current_session_id.as_ref(),
                                                &mut pending_alias,
                                                &mut current_start,
                                                &mut event_log,
                                                &mut subscribers,
                                                &mut rx,
                                            )
                                            .await;
                                            return;
                                        }
                                    }
                                }
                                AgentInput::EditQueuedMessage(payload) => {
                                    let Some(entry) =
                                        queue.iter_mut().find(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    entry.message = payload.message;
                                    entry.images = payload.images;
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                    )
                                    .await;
                                }
                                AgentInput::CancelQueuedMessage(payload) => {
                                    let Some(index) =
                                        queue.iter().position(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let removed = queue.remove(index);
                                    assert!(
                                        removed.is_some(),
                                        "queue remove failed for index {index} after position()"
                                    );
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                    )
                                    .await;
                                }
                                AgentInput::SendQueuedMessageNow(payload) => {
                                    let Some(index) =
                                        queue.iter().position(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let queued = queue
                                        .remove(index)
                                        .expect("queue remove failed after position()");
                                    queue.push_front(queued);
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                    )
                                    .await;

                                    if in_turn {
                                        if !backend
                                            .as_ref()
                                            .expect("backend must exist while actor is running")
                                            .interrupt()
                                            .await
                                        {
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend does not support interrupt"
                                                    .to_owned(),
                                                fatal: false,
                                            };
                                            append_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                FrameKind::AgentError,
                                                &payload,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    let queued = queue
                                        .pop_front()
                                        .expect("queue front must exist after push_front");
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                    )
                                    .await;
                                    in_turn = true;
                                    turn_completion.reset();
                                    if !backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send(AgentInput::SendMessage(
                                            queued_message_to_send_payload(queued),
                                        ))
                                        .await
                                    {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: "agent backend closed".to_owned(),
                                            fatal: true,
                                        };
                                        enter_terminal_failure(
                                            TerminalFailureContext {
                                                accepting_input: &accepting_input_task,
                                                status_handle: &status_handle,
                                                canonical_stream: &canonical_stream,
                                                event_log: &mut event_log,
                                                subscribers: &mut subscribers,
                                                queue: &mut queue,
                                                start: &current_start,
                                                child_completion_tx: &child_completion_tx,
                                            },
                                            &payload,
                                        )
                                        .await;
                                        park_terminal_agent(
                                            &session_store,
                                            current_session_id.as_ref(),
                                            &mut pending_alias,
                                            &mut current_start,
                                            &mut event_log,
                                            &mut subscribers,
                                            &mut rx,
                                        )
                                        .await;
                                        return;
                                    }
                                }
                                AgentInput::UpdateSessionSettings(update) => {
                                    let Some(session_schema) = session_schema.as_ref() else {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: "session settings schema unavailable".to_owned(),
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        continue;
                                    };
                                    if let Err(err) =
                                        validate_session_settings_values(session_schema, &update.values)
                                    {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: err,
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        continue;
                                    }
                                    apply_session_settings_update(
                                        &mut current_session_settings,
                                        &update.values,
                                    );
                                    let _ = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send(AgentInput::UpdateSessionSettings(update))
                                        .await;
                                    if let Err(err) = session_store
                                        .lock()
                                        .await
                                        .set_session_settings(
                                            current_session_id
                                                .as_ref()
                                                .expect("live agent must have session_id"),
                                            current_session_settings.clone(),
                                        )
                                    {
                                        tracing::error!(
                                            "failed to persist session settings for {}: {}",
                                            current_session_id
                                                .as_ref()
                                                .expect("live agent must have session_id"),
                                            err
                                        );
                                    }
                                    append_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        FrameKind::SessionSettings,
                                        &SessionSettingsPayload {
                                            values: current_session_settings.clone(),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                        AgentCommand::EnqueueAutoFollowUp { message } => {
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                continue;
                            }
                            queue.push_back(QueuedMessageEntry {
                                id: QueuedMessageId(Uuid::new_v4().to_string()),
                                message,
                                images: Vec::new(),
                            });
                            update_queued_messages_snapshot(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &queue,
                            )
                            .await;

                            if in_turn {
                                continue;
                            }

                            let queued = queue
                                .pop_front()
                                .expect("queue front must exist after auto-follow-up enqueue");
                            update_queued_messages_snapshot(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &queue,
                            )
                            .await;
                            in_turn = true;
                            turn_completion.reset();
                            if !backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .send(AgentInput::SendMessage(queued_message_to_send_payload(
                                    queued,
                                )))
                                .await
                            {
                                let payload = AgentErrorPayload {
                                    agent_id: current_start.agent_id.clone(),
                                    code: AgentErrorCode::Internal,
                                    message: "agent backend closed".to_owned(),
                                    fatal: true,
                                };
                                enter_terminal_failure(
                                    TerminalFailureContext {
                                        accepting_input: &accepting_input_task,
                                        status_handle: &status_handle,
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        subscribers: &mut subscribers,
                                        queue: &mut queue,
                                        start: &current_start,
                                        child_completion_tx: &child_completion_tx,
                                    },
                                    &payload,
                                )
                                .await;
                                park_terminal_agent(
                                    &session_store,
                                    current_session_id.as_ref(),
                                    &mut pending_alias,
                                    &mut current_start,
                                    &mut event_log,
                                    &mut subscribers,
                                    &mut rx,
                                )
                                .await;
                                return;
                            }
                        }
                        AgentCommand::SetName {
                            name,
                            persistence,
                            reply,
                        } => {
                            let applied = apply_agent_name_change(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: current_session_id.as_ref(),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                name,
                                persistence,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::Snapshot { reply } => {
                            let _ = reply.send(current_start.clone());
                        }
                        AgentCommand::Interrupt => {
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                continue;
                            }
                            if !backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .interrupt()
                                .await
                            {
                                let payload = AgentErrorPayload {
                                    agent_id: current_start.agent_id.clone(),
                                    code: AgentErrorCode::Internal,
                                    message: "agent backend does not support interrupt".to_owned(),
                                    fatal: false,
                                };
                                append_event(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    FrameKind::AgentError,
                                    &payload,
                                )
                                .await;
                            }
                        }
                        AgentCommand::Close { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(());
                                continue;
                            }
                            lifecycle = ActorLifecycle::Closing;
                            close_reply = Some(reply);
                            if !queue.is_empty() {
                                queue.clear();
                                update_queued_messages_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &queue,
                                )
                                .await;
                            }
                            if !in_turn {
                                let reply = close_reply
                                    .take()
                                    .expect("close requested without pending close reply");
                                let backend = backend
                                    .take()
                                    .expect("backend must exist while closing a live actor");
                                shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                        }
                        AgentCommand::Attach(stream) => {
                            attach_subscriber(&event_log, &mut subscribers, stream).await;
                        }
                    }
                }
            }
        }
    });

    (
        AgentHandle {
            tx,
            accepting_input,
        },
        startup_rx,
    )
}

pub(crate) fn spawn_relay_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    mut events: mpsc::UnboundedReceiver<ChatEvent>,
    session_store: Arc<Mutex<SessionStore>>,
    session_id: SessionId,
    status_handle: registry::AgentStatusHandle,
) -> AgentHandle {
    let (tx, mut rx) = mpsc::channel::<AgentCommand>(COMMAND_BUFFER);
    let accepting_input = Arc::new(AtomicBool::new(true));
    let accepting_input_task = Arc::clone(&accepting_input);

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut current_start = start;
        let mut pending_alias = None;
        let mut in_turn = false;
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;

        status_handle
            .update(|s| {
                s.started = true;
                s.last_error = None;
                s.activity_counter = s.activity_counter.saturating_add(1);
            })
            .await;
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &current_start,
        )
        .await;

        loop {
            tokio::select! {
                maybe_event = events.recv() => {
                    let Some(event) = maybe_event else {
                        if matches!(lifecycle, ActorLifecycle::Closing) {
                            let reply = close_reply
                                .take()
                                .expect("close requested without pending close reply");
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
                        }
                        accepting_input_task.store(false, Ordering::SeqCst);
                        status_handle.update(|s| {
                            s.terminated = true;
                            s.is_thinking = false;
                            s.turn_completed = true;
                            s.activity_counter = s.activity_counter.saturating_add(1);
                        }).await;
                        return;
                    };

                    match &event {
                        ChatEvent::MessageAdded(message) => {
                            if matches!(message.sender, MessageSender::Error) {
                                let msg = message.content.clone();
                                status_handle.update(|s| {
                                    s.turn_completed = true;
                                    s.last_error = Some(msg);
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            } else {
                                status_handle.update(|s| {
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            }
                        }
                        ChatEvent::StreamStart(_) => {
                            active_stream_text.clear();
                            in_turn = true;
                            status_handle.update(|s| {
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
                        ChatEvent::StreamEnd(data) => {
                            active_stream_text.clear();
                            let msg = data.message.content.clone();
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_message = Some(msg);
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::TypingStatusChanged(typing) => {
                            let typing = *typing;
                            in_turn = typing;
                            status_handle.update(|s| {
                                s.is_thinking = typing;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(data) => {
                            let msg = data.message.clone();
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_message = Some(msg);
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        _ => {
                            status_handle.update(|s| {
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                    }

                    apply_runtime_session_updates(&session_store, &session_id, &event).await;
                    append_event(
                        &canonical_stream,
                        &mut event_log,
                        &mut subscribers,
                        FrameKind::ChatEvent,
                        &event,
                    )
                    .await;

                    if matches!(event, ChatEvent::TypingStatusChanged(false))
                        && matches!(lifecycle, ActorLifecycle::Closing)
                    {
                        let reply = close_reply
                            .take()
                            .expect("close requested without pending close reply");
                        finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                        return;
                    }
                }
                maybe_command = rx.recv() => {
                    let Some(command) = maybe_command else {
                        return;
                    };
                    match command {
                        AgentCommand::SendInput(_)
                        | AgentCommand::Interrupt
                        | AgentCommand::EnqueueAutoFollowUp { .. } => {
                            let payload = relay_input_rejected_payload(&current_start.agent_id);
                            append_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                FrameKind::AgentError,
                                &payload,
                            )
                            .await;
                        }
                        AgentCommand::SetName {
                            name,
                            persistence,
                            reply,
                        } => {
                            let applied = apply_agent_name_change(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: Some(&session_id),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                name,
                                persistence,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::Snapshot { reply } => {
                            let _ = reply.send(current_start.clone());
                        }
                        AgentCommand::Close { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(());
                                continue;
                            }
                            lifecycle = ActorLifecycle::Closing;
                            close_reply = Some(reply);
                            if !in_turn {
                                let reply = close_reply
                                    .take()
                                    .expect("close requested without pending close reply");
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                        }
                        AgentCommand::Attach(stream) => {
                            attach_subscriber(&event_log, &mut subscribers, stream).await;
                        }
                    }
                }
            }
        }
    });

    AgentHandle {
        tx,
        accepting_input,
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
}

async fn shutdown_backend_with_timeout(backend: BackendHandle, agent_id: &AgentId) {
    if tokio::time::timeout(BACKEND_SHUTDOWN_TIMEOUT, backend.shutdown())
        .await
        .is_err()
    {
        tracing::error!(
            agent_id = %agent_id,
            timeout_ms = BACKEND_SHUTDOWN_TIMEOUT.as_millis(),
            "timed out shutting down backend"
        );
    }
}

async fn finish_actor_close(
    accepting_input: &Arc<AtomicBool>,
    status_handle: &registry::AgentStatusHandle,
    reply: oneshot::Sender<()>,
) {
    accepting_input.store(false, Ordering::SeqCst);
    status_handle
        .update(|s| {
            s.terminated = true;
            s.is_thinking = false;
            s.turn_completed = true;
            s.activity_counter = s.activity_counter.saturating_add(1);
        })
        .await;
    let _ = reply.send(());
}

fn relay_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "backend-native relay agents do not accept direct input".to_owned(),
        fatal: false,
    }
}

fn maybe_emit_child_completion_notice(
    child_completion_tx: &HostChildCompletionNoticeTx,
    start: &AgentStartPayload,
    outcome: ChildCompletionOutcome,
    message_text: String,
) {
    let Some(parent_id) = start.parent_agent_id.clone() else {
        return;
    };
    if start.origin == AgentOrigin::BackendNative {
        // Backend-native children deliver their final response to the parent
        // through the backend's own tool-result mechanism; Tyde does not enqueue
        // a separate completion notice.
        return;
    }
    if message_text.trim().is_empty() {
        return;
    }
    let _ = child_completion_tx.send(ChildCompletionNotice {
        parent_id,
        child_id: start.agent_id.clone(),
        child_name: start.name.clone(),
        outcome,
        message_text,
    });
}

async fn enter_terminal_failure(context: TerminalFailureContext<'_>, payload: &AgentErrorPayload) {
    context.accepting_input.store(false, Ordering::SeqCst);
    context.queue.clear();
    context
        .status_handle
        .update(|s| {
            s.terminated = true;
            s.is_thinking = false;
            s.turn_completed = true;
            s.last_error = Some(payload.message.clone());
            s.activity_counter = s.activity_counter.saturating_add(1);
        })
        .await;
    update_queued_messages_snapshot(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        context.queue,
    )
    .await;
    append_event(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        FrameKind::AgentError,
        payload,
    )
    .await;
    if payload.fatal {
        maybe_emit_child_completion_notice(
            context.child_completion_tx,
            context.start,
            ChildCompletionOutcome::Failed,
            payload.message.clone(),
        );
    }
}

async fn park_terminal_agent(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: Option<&SessionId>,
    pending_alias: &mut Option<InitialAgentAlias>,
    current_start: &mut AgentStartPayload,
    event_log: &mut [Envelope],
    subscribers: &mut Vec<Stream>,
    rx: &mut mpsc::Receiver<AgentCommand>,
) {
    loop {
        let Some(command) = rx.recv().await else {
            break;
        };
        match command {
            AgentCommand::SetName {
                name,
                persistence,
                reply,
            } => {
                let applied = apply_agent_name_change(
                    AgentNameChangeContext {
                        session_store,
                        session_id,
                        pending_alias,
                        current_start,
                        event_log,
                        subscribers,
                    },
                    name,
                    persistence,
                )
                .await;
                let _ = reply.send(applied);
            }
            AgentCommand::Snapshot { reply } => {
                let _ = reply.send(current_start.clone());
            }
            AgentCommand::Attach(stream) => {
                attach_subscriber(event_log, subscribers, stream).await;
            }
            AgentCommand::Close { reply } => {
                let _ = reply.send(());
                break;
            }
            AgentCommand::SendInput(_)
            | AgentCommand::Interrupt
            | AgentCommand::EnqueueAutoFollowUp { .. } => {}
        }
    }
}

async fn apply_agent_name_change(
    context: AgentNameChangeContext<'_>,
    name: String,
    persistence: AgentNamePersistence,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }

    let persisted = if let Some(session_id) = context.session_id {
        let persist_result = {
            let store = context.session_store.lock().await;
            match persistence {
                AgentNamePersistence::User => store
                    .set_user_alias(session_id, trimmed.to_string())
                    .map(|_| true),
                AgentNamePersistence::GeneratedIfNoUserAlias => {
                    store.set_generated_alias_if_no_user_alias(session_id, trimmed.to_string())
                }
            }
        };
        match persist_result {
            Ok(persisted) => persisted,
            Err(err) => {
                tracing::error!(
                    "failed to persist renamed agent {}: {}",
                    context.current_start.agent_id,
                    err
                );
                let payload = AgentErrorPayload {
                    agent_id: context.current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: format!("failed to persist agent name: {err}"),
                    fatal: false,
                };
                broadcast_live_event(context.subscribers, FrameKind::AgentError, &payload).await;
                return false;
            }
        }
    } else {
        match persistence {
            AgentNamePersistence::User => {
                *context.pending_alias = Some(InitialAgentAlias {
                    name: trimmed.to_string(),
                    persistence: InitialAgentAliasPersistence::User,
                });
                true
            }
            AgentNamePersistence::GeneratedIfNoUserAlias => {
                if context.pending_alias.as_ref().is_some_and(|alias| {
                    matches!(alias.persistence, InitialAgentAliasPersistence::User)
                }) {
                    false
                } else {
                    *context.pending_alias = Some(InitialAgentAlias {
                        name: trimmed.to_string(),
                        persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                    });
                    true
                }
            }
        }
    };
    if !persisted {
        return false;
    }

    if context.current_start.name == trimmed {
        return true;
    }

    context.current_start.name = trimmed.to_string();
    overwrite_agent_start_payload(context.event_log, context.current_start);

    let payload = AgentRenamedPayload {
        agent_id: context.current_start.agent_id.clone(),
        name: context.current_start.name.clone(),
    };
    broadcast_live_event(context.subscribers, FrameKind::AgentRenamed, &payload).await;
    true
}

fn overwrite_agent_start_payload(event_log: &mut [Envelope], current_start: &AgentStartPayload) {
    let Some(first) = event_log.first_mut() else {
        panic!("agent replay log is empty; AgentStart must always be present");
    };
    assert_eq!(
        first.kind,
        FrameKind::AgentStart,
        "agent replay log must begin with AgentStart"
    );
    first.payload = serde_json::to_value(current_start)
        .expect("failed to serialize updated AgentStart payload");
}

fn queued_message_to_send_payload(entry: QueuedMessageEntry) -> SendMessagePayload {
    SendMessagePayload {
        message: entry.message,
        images: (!entry.images.is_empty()).then_some(entry.images),
    }
}

async fn emit_unknown_queued_message_error(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    agent_id: &AgentId,
    queued_message_id: &QueuedMessageId,
) {
    let payload = AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: format!("unknown queued message id {}", queued_message_id),
        fatal: false,
    };
    append_event(
        canonical_stream,
        event_log,
        subscribers,
        FrameKind::AgentError,
        &payload,
    )
    .await;
}

async fn persist_agent_session(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: &SessionId,
    parent_session_id: Option<SessionId>,
    current_start: &AgentStartPayload,
    current_session_settings: &SessionSettingsValues,
    pending_alias: &mut Option<InitialAgentAlias>,
) -> Result<(), String> {
    let session = BackendSession {
        id: session_id.clone(),
        backend_kind: current_start.backend_kind,
        workspace_roots: current_start.workspace_roots.clone(),
        title: None,
        token_count: None,
        created_at_ms: Some(current_start.created_at_ms),
        updated_at_ms: Some(current_start.created_at_ms),
        resumable: current_start.origin != AgentOrigin::BackendNative,
    };

    {
        let store = session_store.lock().await;
        store.upsert_backend_session(
            &session,
            parent_session_id,
            current_start.project_id.clone(),
            current_start.custom_agent_id.clone(),
        )?;
        store.set_session_settings(session_id, current_session_settings.clone())?;
        if let Some(alias) = pending_alias.take() {
            match alias.persistence {
                InitialAgentAliasPersistence::GeneratedIfNoUserAlias => {
                    let _ = store.set_generated_alias_if_no_user_alias(session_id, alias.name)?;
                }
                InitialAgentAliasPersistence::User => {
                    store.set_user_alias(session_id, alias.name)?;
                }
            }
        }
    }

    Ok(())
}

async fn append_event<T: serde::Serialize>(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    kind: FrameKind,
    payload: &T,
) {
    let seq = event_log.len() as u64;
    let event = Envelope::from_payload(
        protocol::StreamPath(canonical_stream.to_owned()),
        kind,
        seq,
        payload,
    )
    .expect("failed to serialize protocol payload in agent actor");
    event_log.push(event.clone());
    broadcast_event(subscribers, &event).await;
}

async fn update_queued_messages_snapshot(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    queue: &VecDeque<QueuedMessageEntry>,
) {
    let payload = QueuedMessagesPayload {
        messages: queue.iter().cloned().collect(),
    };
    let value =
        serde_json::to_value(&payload).expect("failed to serialize queued messages payload");

    if let Some(snapshot) = event_log
        .iter_mut()
        .find(|event| event.kind == FrameKind::QueuedMessages)
    {
        snapshot.payload = value.clone();
    } else {
        event_log.push(Envelope {
            stream: protocol::StreamPath(canonical_stream.to_owned()),
            kind: FrameKind::QueuedMessages,
            seq: event_log.len() as u64,
            payload: value.clone(),
        });
    }

    broadcast_live_event(subscribers, FrameKind::QueuedMessages, &payload).await;
}

async fn broadcast_live_event<T: serde::Serialize>(
    subscribers: &mut Vec<Stream>,
    kind: FrameKind,
    payload: &T,
) {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize live protocol payload in agent actor");
    let event = Envelope {
        stream: protocol::StreamPath(String::new()),
        kind,
        seq: 0,
        payload,
    };
    broadcast_event(subscribers, &event).await;
}

async fn broadcast_event(subscribers: &mut Vec<Stream>, event: &Envelope) {
    let mut idx = 0;
    while idx < subscribers.len() {
        if subscribers[idx]
            .send_value(event.kind, event.payload.clone())
            .await
            .is_err()
        {
            subscribers.swap_remove(idx);
            continue;
        }
        idx += 1;
    }
}

async fn attach_subscriber(event_log: &[Envelope], subscribers: &mut Vec<Stream>, stream: Stream) {
    for event in event_log {
        if stream
            .send_value(event.kind, event.payload.clone())
            .await
            .is_err()
        {
            return;
        }
    }

    subscribers.push(stream);
}

async fn apply_runtime_session_updates(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: &SessionId,
    event: &ChatEvent,
) {
    let result = {
        let store = session_store.lock().await;
        match event {
            ChatEvent::StreamEnd(data) => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
                record.message_count += 1;
                if let Some(delta) = data
                    .message
                    .token_usage
                    .as_ref()
                    .map(|usage| usage.total_tokens)
                {
                    record.token_count =
                        Some(record.token_count.unwrap_or(0).saturating_add(delta));
                }
            }),
            ChatEvent::TaskUpdate(tasks) => {
                let title = tasks.title.trim();
                store.update(session_id, |record| {
                    record.updated_at_ms = now_ms();
                    if !title.is_empty() && record.alias.is_none() {
                        record.alias = Some(title.to_string());
                    }
                })
            }
            _ => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
            }),
        }
    };

    if let Err(err) = result {
        tracing::error!("failed to update session store for {}: {}", session_id, err);
    }
}

fn build_name_generation_prompt(prompt: &str) -> String {
    format!(
        "Return only a short 2-4 word work name for this request. No quotes, no markdown, no explanation. Request: {prompt}"
    )
}

pub(crate) fn derive_agent_name(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return IMAGE_ONLY_AGENT_NAME.to_string();
    }

    generate_mock_name(trimmed).unwrap_or_else(|fallback_err| {
        tracing::error!(
            "prompt-derived agent name fallback failed for prompt {:?}: {}",
            trimmed,
            fallback_err
        );
        IMAGE_ONLY_AGENT_NAME.to_string()
    })
}

fn generate_mock_name(prompt: &str) -> Result<String, String> {
    let mut words = extract_name_words(prompt);
    if words.is_empty() {
        words = vec!["New".to_string(), "Agent".to_string(), "Task".to_string()];
    }
    words.truncate(4);
    while words.len() < 2 {
        words.push("Task".to_string());
    }
    sanitize_generated_agent_name(&words.join(" "))
}

fn name_generation_fallback(prompt: &str, generated: &str) -> String {
    match sanitize_generated_agent_name(generated) {
        Ok(name) => name,
        Err(err) => {
            tracing::warn!(
                "agent name generator produced invalid output {:?}: {}; falling back to prompt-derived name",
                generated,
                err
            );
            derive_agent_name(prompt)
        }
    }
}

fn sanitize_generated_agent_name(name: &str) -> Result<String, String> {
    let stripped = strip_wrapping_quotes(name.trim());
    if stripped.is_empty() {
        return Err("generated agent name was empty".to_string());
    }

    let mut words = stripped
        .split_whitespace()
        .map(clean_name_word)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    if words.len() < 2 || words.len() > 4 {
        return Err(format!(
            "generated agent name must contain 2-4 words, got {:?}",
            stripped
        ));
    }

    for word in &mut words {
        *word = title_case_word(word);
    }

    Ok(words.join(" "))
}

fn strip_wrapping_quotes(mut value: &str) -> &str {
    loop {
        let trimmed = value.trim();
        let bytes = trimmed.as_bytes();
        if bytes.len() < 2 {
            return trimmed;
        }
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        let wrapped = matches!((first, last), ('\"', '\"') | ('\'', '\'') | ('`', '`'));
        if !wrapped {
            return trimmed;
        }
        value = &trimmed[1..trimmed.len() - 1];
    }
}

fn extract_name_words(prompt: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "at", "based", "by", "for", "from", "how", "i", "if", "in", "into",
        "make", "new", "of", "on", "or", "please", "so", "that", "the", "this", "to", "update",
        "with", "you",
    ];

    let mut words = Vec::new();
    for raw in prompt.split_whitespace() {
        let cleaned = clean_name_word(raw);
        if cleaned.is_empty() {
            continue;
        }
        if STOPWORDS.contains(&cleaned.to_ascii_lowercase().as_str()) {
            continue;
        }
        words.push(title_case_word(&cleaned));
    }
    words
}

fn clean_name_word(word: &str) -> String {
    word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

fn title_case_word(word: &str) -> String {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_uppercase());
    out.push_str(&chars.as_str().to_ascii_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use protocol::{AgentInput, AgentStartPayload, FrameKind, StreamPath};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::{
        AgentCommand, AgentHandle, append_event, attach_subscriber, generate_mock_name,
        name_generation_fallback, sanitize_generated_agent_name,
    };
    use crate::agent::registry::AgentStatusHandle;
    use crate::stream::Stream;

    fn spawn_failed_agent_actor(
        start: AgentStartPayload,
        error: String,
        status_handle: AgentStatusHandle,
    ) -> AgentHandle {
        let (tx, mut rx) = mpsc::channel::<AgentCommand>(8);
        let accepting_input = Arc::new(AtomicBool::new(false));
        let accepting_input_task = Arc::clone(&accepting_input);

        tokio::spawn(async move {
            let payload = protocol::AgentErrorPayload {
                agent_id: start.agent_id.clone(),
                code: protocol::AgentErrorCode::BackendFailed,
                message: error,
                fatal: true,
            };
            status_handle
                .update(|s| {
                    s.terminated = true;
                    s.turn_completed = true;
                    s.last_error = Some(payload.message.clone());
                    s.activity_counter = s.activity_counter.saturating_add(1);
                })
                .await;

            let mut event_log = Vec::new();
            let mut subscribers = Vec::new();
            append_event(
                &format!("/agent/{}", start.agent_id),
                &mut event_log,
                &mut subscribers,
                FrameKind::AgentError,
                &payload,
            )
            .await;

            while let Some(command) = rx.recv().await {
                match command {
                    AgentCommand::Snapshot { reply } => {
                        let _ = reply.send(start.clone());
                    }
                    AgentCommand::Attach(stream) => {
                        attach_subscriber(&event_log, &mut subscribers, stream).await;
                    }
                    AgentCommand::SetName { reply, .. } => {
                        let _ = reply.send(false);
                    }
                    AgentCommand::Close { reply } => {
                        let _ = reply.send(());
                        break;
                    }
                    AgentCommand::SendInput(_)
                    | AgentCommand::Interrupt
                    | AgentCommand::EnqueueAutoFollowUp { .. } => {}
                }
            }
        });

        AgentHandle {
            tx,
            accepting_input: accepting_input_task,
        }
    }

    #[test]
    fn generated_name_sanitizer_accepts_valid_name() {
        assert_eq!(
            sanitize_generated_agent_name("  \"fix login flow\" ").unwrap(),
            "Fix Login Flow"
        );
    }

    #[test]
    fn name_generation_fallback_uses_prompt_when_generated_name_is_empty() {
        assert_eq!(
            name_generation_fallback("please fix login flow", "   "),
            "Fix Login Flow"
        );
    }

    #[test]
    fn name_generation_fallback_uses_prompt_when_generated_name_has_wrong_shape() {
        assert_eq!(
            name_generation_fallback("add project search filter", "project"),
            "Add Project Search Filter"
        );
    }

    #[test]
    fn mock_name_uses_default_words_when_prompt_has_no_name_words() {
        assert_eq!(generate_mock_name("!!!").unwrap(), "New Agent Task");
    }

    #[tokio::test]
    async fn failed_agent_actor_replays_terminal_error_and_rejects_input() {
        let start = AgentStartPayload {
            agent_id: protocol::AgentId("agent-failed".to_string()),
            name: "Chat".to_string(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Tycode,
            workspace_roots: vec!["/tmp/test".to_string()],
            custom_agent_id: None,
            project_id: None,
            parent_agent_id: None,
            created_at_ms: 1,
        };
        let (status_handle, _rx) = AgentStatusHandle::new();
        let handle =
            spawn_failed_agent_actor(start.clone(), "backend blew up".to_string(), status_handle);

        assert!(
            !handle
                .send_input(AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "hello".to_string(),
                    images: None,
                }))
                .await
        );
        let snapshot = handle
            .snapshot()
            .await
            .expect("snapshot should survive failure");
        assert_eq!(snapshot.agent_id.0, "agent-failed");
        assert_eq!(snapshot.name, "Chat");

        let (tx, mut rx) = mpsc::channel(8);
        let stream = Stream::new(StreamPath("/agent/agent-failed".to_string()), tx);
        assert!(handle.attach(stream).await);

        let env = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for AgentError")
            .expect("agent stream closed before AgentError");
        assert_eq!(env.kind, FrameKind::AgentError);
        let payload: protocol::AgentErrorPayload =
            serde_json::from_value(env.payload).expect("parse AgentError");
        assert!(payload.fatal);
        assert_eq!(payload.message, "backend blew up");
    }
}
