use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentStartPayload, BackendKind,
    ChatEvent, Envelope, FrameKind, SessionId,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::backend::claude::ClaudeBackend;
use crate::backend::codex::CodexBackend;
use crate::backend::gemini::GeminiBackend;
use crate::backend::mock::MockBackend;
use crate::backend::{Backend, BackendSpawnConfig, EventStream};
use crate::store::session::SessionStore;
use crate::stream::Stream;

pub(crate) mod registry;

use self::registry::ResolvedSpawnRequest;

const COMMAND_BUFFER: usize = 64;

enum AgentCommand {
    SendInput(AgentInput),
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

    pub async fn attach(&self, stream: Stream) -> bool {
        self.tx.send(AgentCommand::Attach(stream)).await.is_ok()
    }
}

/// Type-erased backend handle. The actor loop only needs `send()` — this lets
/// us dispatch to any concrete `Backend` at spawn time and forget the type.
trait BackendSender: Send + 'static {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
}

impl<B: Backend> BackendSender for B {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::send(self, input))
    }
}

/// Spawn the correct backend based on `backend_kind`.
/// Session ID is generated here (UUID) — real backends don't know their session ID at spawn time.
/// The backend starts idle — the caller sends the first message via `send()`.
async fn spawn_backend(
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
) -> Result<(Box<dyn BackendSender>, EventStream, SessionId), String> {
    let session_id = SessionId(Uuid::new_v4().to_string());
    let (backend, events): (Box<dyn BackendSender>, EventStream) = match backend_kind {
        BackendKind::Claude => {
            let (b, events) = ClaudeBackend::spawn(workspace_roots, config).await?;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) = CodexBackend::spawn(workspace_roots, config).await?;
            (Box::new(b), events)
        }
        BackendKind::Gemini => {
            let (b, events) = GeminiBackend::spawn(workspace_roots, config).await?;
            (Box::new(b), events)
        }
    };
    Ok((backend, events, session_id))
}

async fn resume_backend(
    backend_kind: BackendKind,
    session_id: SessionId,
) -> Result<(Box<dyn BackendSender>, EventStream), String> {
    match backend_kind {
        BackendKind::Claude => {
            let (b, events) = ClaudeBackend::resume(session_id).await?;
            Ok((Box::new(b), events))
        }
        BackendKind::Codex => {
            let (b, events) = CodexBackend::resume(session_id).await?;
            Ok((Box::new(b), events))
        }
        BackendKind::Gemini => {
            let (b, events) = GeminiBackend::resume(session_id).await?;
            Ok((Box::new(b), events))
        }
    }
}

fn spawn_mock(
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(Box<dyn BackendSender>, EventStream, SessionId), String>> + Send>,
> {
    Box::pin(async move {
        let (b, events) = MockBackend::spawn(workspace_roots, config).await?;
        let sid = b.session_id();
        Ok((Box::new(b) as Box<dyn BackendSender>, events, sid))
    })
}

fn resume_mock(
    session_id: SessionId,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(Box<dyn BackendSender>, EventStream), String>> + Send>,
> {
    Box::pin(async move {
        let (b, events) = MockBackend::resume(session_id).await?;
        Ok((Box::new(b) as Box<dyn BackendSender>, events))
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
    };
    let use_mock = request.use_mock_backend;
    let (backend, mut events, session_id, initial_follow_up): (
        Box<dyn BackendSender>,
        EventStream,
        SessionId,
        Option<String>,
    ) = match request.resume_session_id {
        Some(session_id) => {
            let (backend, events) = if use_mock {
                resume_mock(session_id.clone()).await?
            } else {
                resume_backend(request.backend_kind, session_id.clone()).await?
            };
            (backend, events, session_id, request.initial_prompt)
        }
        None => {
            let (backend, events, session_id) = if use_mock {
                spawn_mock(request.workspace_roots, spawn_config).await?
            } else {
                spawn_backend(
                    request.backend_kind,
                    request.workspace_roots,
                    spawn_config,
                )
                .await?
            };
            (backend, events, session_id, request.initial_prompt)
        }
    };

    let (tx, mut rx) = mpsc::channel::<AgentCommand>(COMMAND_BUFFER);
    let actor_session_id = session_id.clone();

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut subscribers: Vec<Stream> = Vec::new();

        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &start,
        )
        .await;

        if let Some(prompt) = initial_follow_up.filter(|p| !p.is_empty()) {
            let sent = backend
                .send(AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: prompt,
                }))
                .await;
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
                    apply_runtime_session_updates(
                        &session_store,
                        &actor_session_id,
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
                        AgentCommand::Attach(stream) => {
                            attach_subscriber(&event_log, &mut subscribers, stream).await;
                        }
                    }
                }
            }
        }
    });

    Ok((AgentHandle { tx }, session_id))
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
