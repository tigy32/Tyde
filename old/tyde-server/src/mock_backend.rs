use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use tokio::sync::mpsc;
use tyde_protocol::protocol::{
    ChatEvent, ChatMessage, MessageSender, StreamEndData, StreamStartData,
};

use crate::agent::{AgentHandle, Backend, CommandExecutor};
use crate::backends::types::SessionCommand;

#[derive(Debug, Clone, Default)]
pub enum MockBehavior {
    #[default]
    Echo,
    Events(Vec<ChatEvent>),
    Crash,
    Silent,
}

struct MockBackendInner {
    behavior: MockBehavior,
    event_tx: Option<mpsc::UnboundedSender<ChatEvent>>,
    captured_commands: Vec<SessionCommand>,
}

#[derive(Clone)]
pub struct MockCommandHandle {
    inner: Arc<SyncMutex<MockBackendInner>>,
}

impl CommandExecutor for MockCommandHandle {
    fn execute(
        &self,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (behavior, tx) = {
                let mut guard = inner.lock();
                guard.captured_commands.push(command.clone());
                (guard.behavior.clone(), guard.event_tx.clone())
            };

            let Some(tx) = tx else {
                return Err("Mock backend event channel closed".to_string());
            };

            match behavior {
                MockBehavior::Echo => {
                    let message_text = match &command {
                        SessionCommand::SendMessage { message, .. } => message.clone(),
                        other => format!("{other:?}"),
                    };
                    for event in echo_events(&message_text) {
                        let _ = tx.send(event);
                    }
                }
                MockBehavior::Events(events) => {
                    for event in events {
                        let _ = tx.send(event);
                    }
                }
                MockBehavior::Crash => {
                    inner.lock().event_tx = None;
                }
                MockBehavior::Silent => {}
            }

            Ok(())
        })
    }
}

pub struct MockSession {
    inner: Arc<SyncMutex<MockBackendInner>>,
}

impl Backend for MockSession {
    fn agent_handle(&self) -> AgentHandle {
        AgentHandle::new(Arc::new(MockCommandHandle {
            inner: self.inner.clone(),
        }))
    }

    fn kind_str(&self) -> String {
        "mock".to_string()
    }

    fn tracks_local_session_store(&self) -> bool {
        false
    }

    fn shutdown(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            self.inner.lock().event_tx = None;
        })
    }
}

pub fn spawn_mock_session(
    behavior: MockBehavior,
) -> (MockSession, mpsc::UnboundedReceiver<ChatEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let inner = Arc::new(SyncMutex::new(MockBackendInner {
        behavior,
        event_tx: Some(tx),
        captured_commands: Vec::new(),
    }));
    (MockSession { inner }, rx)
}

pub fn spawn_controlled_mock() -> (
    MockController,
    MockSession,
    mpsc::UnboundedReceiver<ChatEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let inner = Arc::new(SyncMutex::new(MockBackendInner {
        behavior: MockBehavior::Echo,
        event_tx: Some(tx),
        captured_commands: Vec::new(),
    }));
    let controller = MockController {
        inner: inner.clone(),
    };
    let session = MockSession { inner };
    (controller, session, rx)
}

#[derive(Clone)]
pub struct MockController {
    inner: Arc<SyncMutex<MockBackendInner>>,
}

impl MockController {
    pub fn set_behavior(&self, behavior: MockBehavior) {
        self.inner.lock().behavior = behavior;
    }

    pub fn captured_commands(&self) -> Vec<SessionCommand> {
        self.inner.lock().captured_commands.clone()
    }

    pub fn clear_captured_commands(&self) {
        self.inner.lock().captured_commands.clear();
    }
}

fn echo_events(message: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::TypingStatusChanged(true),
        ChatEvent::StreamStart(StreamStartData {
            message_id: None,
            agent: "mock".to_string(),
            model: None,
        }),
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                timestamp: 0,
                sender: MessageSender::Assistant {
                    agent: "mock".to_string(),
                },
                content: message.to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        }),
        ChatEvent::TypingStatusChanged(false),
    ]
}
