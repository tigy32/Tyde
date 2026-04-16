use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentStartPayload, BackendKind,
    ChatEvent, Envelope, FrameKind, SendMessagePayload, SessionId,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::backend::claude::ClaudeBackend;
use crate::backend::codex::CodexBackend;
use crate::backend::gemini::GeminiBackend;
use crate::backend::kiro::KiroBackend;
use crate::backend::mock::MockBackend;
use crate::backend::tycode::TycodeBackend;
use crate::backend::{Backend, BackendSpawnConfig, EventStream};
use crate::store::session::SessionStore;
use crate::stream::Stream;

pub(crate) mod registry;

use self::registry::ResolvedSpawnRequest;

const COMMAND_BUFFER: usize = 64;

type BackendHandle = Box<dyn BackendSender>;
type BackendSpawnResult = Result<(BackendHandle, EventStream, SessionId), String>;
type BackendResumeResult = Result<(BackendHandle, EventStream), String>;
type BackendFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;

enum AgentCommand {
    SendInput(AgentInput),
    Interrupt,
    Attach(Stream),
}

#[derive(Clone)]
pub(crate) struct AgentHandle {
    tx: mpsc::Sender<AgentCommand>,
}

impl AgentHandle {
    pub async fn send_input(&self, input: AgentInput) -> bool {
        self.tx.send(AgentCommand::SendInput(input)).await.is_ok()
    }

    pub async fn interrupt(&self) -> bool {
        self.tx.send(AgentCommand::Interrupt).await.is_ok()
    }

    pub async fn attach(&self, stream: Stream) -> bool {
        self.tx.send(AgentCommand::Attach(stream)).await.is_ok()
    }
}

/// Type-erased backend handle. The actor loop only needs `send()` — this lets
/// us dispatch to any concrete `Backend` at spawn time and forget the type.
trait BackendSender: Send + 'static {
    fn session_id(&self) -> SessionId;
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
}

impl<B: Backend> BackendSender for B {
    fn session_id(&self) -> SessionId {
        Backend::session_id(self)
    }

    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::send(self, input))
    }

    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::interrupt(self))
    }
}

/// Spawn the correct backend based on `backend_kind`.
/// If the backend already knows its native resumable session ID, return it.
async fn spawn_backend(
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
) -> BackendSpawnResult {
    let (backend, events): (BackendHandle, EventStream) = match backend_kind {
        BackendKind::Tycode => {
            let (b, events) = TycodeBackend::spawn(workspace_roots, config, initial_input).await?;
            (Box::new(b), events)
        }
        BackendKind::Kiro => {
            let (b, events) = KiroBackend::spawn(workspace_roots, config, initial_input).await?;
            (Box::new(b), events)
        }
        BackendKind::Claude => {
            let (b, events) = ClaudeBackend::spawn(workspace_roots, config, initial_input).await?;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) = CodexBackend::spawn(workspace_roots, config, initial_input).await?;
            (Box::new(b), events)
        }
        BackendKind::Gemini => {
            let (b, events) = GeminiBackend::spawn(workspace_roots, config, initial_input).await?;
            (Box::new(b), events)
        }
    };
    let session_id = backend.session_id();
    Ok((backend, events, session_id))
}

async fn resume_backend(
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    session_id: SessionId,
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
            let (b, events) = ClaudeBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) = CodexBackend::resume(workspace_roots, config, session_id).await?;
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
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
) -> BackendFuture<BackendSpawnResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::spawn(workspace_roots, config, initial_input).await?;
        let sid = Backend::session_id(&b);
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

fn resume_mock(
    workspace_roots: Vec<String>,
    session_id: SessionId,
) -> BackendFuture<BackendResumeResult> {
    Box::pin(async move {
        let (b, events) =
            MockBackend::resume(workspace_roots, BackendSpawnConfig::default(), session_id).await?;
        Ok((Box::new(b) as BackendHandle, events))
    })
}

pub(crate) async fn spawn_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    request: ResolvedSpawnRequest,
    session_store: Arc<Mutex<SessionStore>>,
) -> Result<(AgentHandle, SessionId), String> {
    let spawn_config = BackendSpawnConfig {
        cost_hint: request.cost_hint,
        startup_mcp_servers: request.startup_mcp_servers,
    };
    let use_mock = request.use_mock_backend;
    let (backend, mut events, actor_session_id, initial_follow_up): (
        BackendHandle,
        EventStream,
        SessionId,
        Option<SendMessagePayload>,
    ) = match request.resume_session_id {
        Some(session_id) => {
            let (backend, events) = if use_mock {
                resume_mock(request.workspace_roots.clone(), session_id.clone()).await?
            } else {
                resume_backend(
                    request.backend_kind,
                    request.workspace_roots.clone(),
                    spawn_config.clone(),
                    session_id.clone(),
                )
                .await?
            };
            (backend, events, session_id, request.initial_input)
        }
        None => {
            let initial_input = request
                .initial_input
                .clone()
                .expect("new spawn requires initial_input");
            let (backend, events, session_id) = if use_mock {
                spawn_mock(request.workspace_roots, spawn_config, initial_input).await?
            } else {
                spawn_backend(
                    request.backend_kind,
                    request.workspace_roots,
                    spawn_config,
                    initial_input,
                )
                .await?
            };
            (backend, events, session_id, None)
        }
    };

    let (tx, mut rx) = mpsc::channel::<AgentCommand>(COMMAND_BUFFER);
    let actor_session_id_for_task = actor_session_id.clone();

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();

        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &start,
        )
        .await;

        if let Some(input) = initial_follow_up.filter(|input| {
            !input.message.trim().is_empty()
                || input
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty())
        }) {
            let sent = backend.send(AgentInput::SendMessage(input)).await;
            if !sent {
                let payload = AgentErrorPayload {
                    agent_id: start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: "agent backend closed".to_owned(),
                    fatal: true,
                };
                append_event(
                    &canonical_stream,
                    &mut event_log,
                    &mut subscribers,
                    FrameKind::AgentError,
                    &payload,
                )
                .await;
                return;
            }
        }

        loop {
            tokio::select! {
                maybe_event = events.recv() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                    match &event {
                        ChatEvent::StreamStart(_) => active_stream_text.clear(),
                        ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
                        ChatEvent::StreamEnd(_) => active_stream_text.clear(),
                        _ => {}
                    }
                    apply_runtime_session_updates(
                        &session_store,
                        &actor_session_id_for_task,
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
                }
                maybe_command = rx.recv() => {
                    let Some(command) = maybe_command else {
                        break;
                    };
                    match command {
                        AgentCommand::SendInput(input) => {
                            if !backend.send(input).await {
                                let payload = AgentErrorPayload {
                                    agent_id: start.agent_id.clone(),
                                    code: AgentErrorCode::Internal,
                                    message: "agent backend closed".to_owned(),
                                    fatal: true,
                                };
                                append_event(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    FrameKind::AgentError,
                                    &payload,
                                )
                                .await;
                                break;
                            }
                        }
                        AgentCommand::Interrupt => {
                            if !backend.interrupt().await {
                                let payload = AgentErrorPayload {
                                    agent_id: start.agent_id.clone(),
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
                        AgentCommand::Attach(stream) => {
                            attach_subscriber(&event_log, &mut subscribers, stream).await;
                        }
                    }
                }
            }
        }
    });

    Ok((AgentHandle { tx }, actor_session_id))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
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
    let Some(first) = event_log.first() else {
        panic!("agent replay log is empty; AgentStart must always be present");
    };
    assert_eq!(
        first.kind,
        FrameKind::AgentStart,
        "agent replay log must begin with AgentStart"
    );

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
                    if !title.is_empty() {
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
