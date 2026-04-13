use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, ModelInfo, SessionId,
    StreamEndData, StreamStartData, StreamTextDeltaData,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{Backend, BackendSession, BackendSpawnConfig, EventStream};

const INPUT_BUFFER: usize = 64;
const EVENT_BUFFER: usize = 256;
const MOCK_MODEL: &str = "mock";

#[derive(Debug, Clone)]
struct MockSessionRecord {
    workspace_roots: Vec<String>,
    prompts: Vec<String>,
    created_at_ms: u64,
    updated_at_ms: u64,
}

fn session_store() -> &'static Mutex<HashMap<String, MockSessionRecord>> {
    static STORE: OnceLock<Mutex<HashMap<String, MockSessionRecord>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct MockBackend {
    input_tx: mpsc::Sender<AgentInput>,
    session_id: SessionId,
}

impl Backend for MockBackend {
    async fn spawn(
        workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
    ) -> Result<(Self, EventStream), String> {
        let session_id = SessionId(Uuid::new_v4().to_string());
        let now = now_ms();

        {
            let mut store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            store.insert(
                session_id.0.clone(),
                MockSessionRecord {
                    workspace_roots,
                    prompts: Vec::new(),
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            );
        }

        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let session_id_for_task = session_id.clone();

        tokio::spawn(async move {
            while let Some(input) = input_rx.recv().await {
                match input {
                    AgentInput::SendMessage(payload) => {
                        record_prompt(&session_id_for_task, &payload.message);
                        if !emit_turn(&events_tx, &payload.message).await {
                            return;
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                input_tx,
                session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(session_id: SessionId) -> Result<(Self, EventStream), String> {
        {
            let store = session_store()
                .lock()
                .expect("mock backend session store mutex poisoned");
            if !store.contains_key(&session_id.0) {
                return Err(format!("unknown mock session {}", session_id.0));
            }
        }

        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let session_id_for_task = session_id.clone();

        tokio::spawn(async move {
            while let Some(input) = input_rx.recv().await {
                match input {
                    AgentInput::SendMessage(payload) => {
                        record_prompt(&session_id_for_task, &payload.message);
                        if !emit_turn(&events_tx, &payload.message).await {
                            return;
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                input_tx,
                session_id,
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
        self.input_tx.send(input).await.is_ok()
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

async fn emit_turn(events_tx: &mpsc::Sender<ChatEvent>, user_message: &str) -> bool {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = format!("mock backend response to: {user_message}");

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
        token_usage: None,
        context_breakdown: None,
        images: None,
    };

    events_tx
        .send(ChatEvent::StreamEnd(StreamEndData { message }))
        .await
        .is_ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
}
