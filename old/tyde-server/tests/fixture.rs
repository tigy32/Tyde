use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tyde_protocol::protocol::ChatEvent;

use tyde_server::chat_buffer::ChatEventBuffer;
use tyde_server::mock_backend::{
    spawn_controlled_mock, spawn_mock_session, MockBehavior, MockController,
};
use tyde_server::server_state::ServerState;
use tyde_server::stores::{ProjectStore, SessionStore};

pub struct Fixture {
    pub server: Arc<ServerState>,
    pub chat_events: Arc<SyncMutex<ChatEventBuffer>>,
    _workspace_dir: TempDir,
    workspace_path: String,
}

impl Fixture {
    pub fn new() -> Self {
        let workspace_dir = tempfile::tempdir().expect("create temp workspace");
        let workspace_path = workspace_dir.path().to_string_lossy().to_string();

        let store_dir = tempfile::tempdir().expect("create temp store dir");
        let session_store =
            SessionStore::load(PathBuf::from(store_dir.path()).join("sessions.json"))
                .expect("create session store");
        let project_store =
            ProjectStore::load(PathBuf::from(store_dir.path()).join("projects.json"))
                .expect("create project store");

        let server = Arc::new(ServerState::new(session_store, project_store));
        let chat_events = Arc::new(SyncMutex::new(ChatEventBuffer::new()));

        Self {
            server,
            chat_events,
            _workspace_dir: workspace_dir,
            workspace_path,
        }
    }

    pub fn workspace_path(&self) -> &str {
        &self.workspace_path
    }

    pub async fn create_agent(&self) -> String {
        let (session, rx) = spawn_mock_session(MockBehavior::Echo);

        let info = self
            .server
            .register_agent(
                Box::new(session),
                vec![self.workspace_path.clone()],
                "mock".to_string(),
                None,
                "test-agent".to_string(),
                None,
            )
            .await;

        self.spawn_event_pump(info.agent_id.clone(), rx);
        info.agent_id
    }

    pub async fn create_agent_controlled(&self) -> (String, MockController) {
        let (controller, session, rx) = spawn_controlled_mock();

        let info = self
            .server
            .register_agent(
                Box::new(session),
                vec![self.workspace_path.clone()],
                "mock".to_string(),
                None,
                "test-agent".to_string(),
                None,
            )
            .await;

        self.spawn_event_pump(info.agent_id.clone(), rx);
        (info.agent_id, controller)
    }

    pub fn drain_chat_events(&self, agent_id: &str) -> Vec<Value> {
        let buf = self.chat_events.lock();
        let empty = std::collections::HashMap::new();
        buf.all_events_since(&empty)
            .into_iter()
            .filter(|e| e.agent_id == agent_id)
            .map(|e| serde_json::to_value(&e.event).unwrap_or_default())
            .collect()
    }

    fn spawn_event_pump(&self, agent_id: String, mut rx: mpsc::UnboundedReceiver<ChatEvent>) {
        let chat_events = self.chat_events.clone();
        let agent_registry = self.server.agent_registry.clone();
        let agent_notify = self.server.agent_notify.clone();

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                chat_events.lock().push(agent_id.clone(), event.clone());

                let mut reg = agent_registry.lock().await;
                let changed = reg.record_chat_event(&agent_id, &event);
                drop(reg);
                if changed {
                    agent_notify.notify_waiters();
                }
            }
        });
    }
}
