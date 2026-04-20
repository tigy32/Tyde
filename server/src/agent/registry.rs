use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{
    AgentId, AgentOrigin, AgentStartPayload, BackendKind, ChatEvent, CustomAgentId, ProjectId,
    SendMessagePayload, SessionId, SessionSettingsSchema, SessionSettingsValues, SpawnCostHint,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::agent::customization::ResolvedSpawnConfig;
use crate::agent::{AgentHandle, now_ms, spawn_agent_actor, spawn_relay_agent_actor};
use crate::backend::StartupMcpServer;
use crate::backend::StartupMcpTransport;
use crate::host::HostChildCompletionNoticeTx;
use crate::host::agent_control_mcp_url_for_agent;
use crate::store::session::SessionStore;
use crate::sub_agent::HostSubAgentSpawnTx;

pub(crate) struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
    status_change_tx: watch::Sender<u64>,
    status_change_counter: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentStatus {
    pub started: bool,
    pub terminated: bool,
    pub is_thinking: bool,
    pub turn_completed: bool,
    pub last_message: Option<String>,
    pub last_error: Option<String>,
    pub activity_counter: u64,
}

impl AgentStatus {
    pub fn is_active(&self) -> bool {
        !self.terminated && (!self.started || self.is_thinking || !self.turn_completed)
    }

    pub fn status_label(&self) -> &'static str {
        if self.terminated && self.last_error.is_some() {
            "error"
        } else if self.is_active() {
            "thinking"
        } else {
            "idle"
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentStatusHandle {
    status: Arc<Mutex<AgentStatus>>,
    status_change_tx: watch::Sender<u64>,
    status_change_counter: Arc<AtomicU64>,
}

impl AgentStatusHandle {
    #[cfg(test)]
    pub fn new() -> (Self, watch::Receiver<u64>) {
        let (status_change_tx, status_change_rx) = watch::channel(0);
        let status_change_counter = Arc::new(AtomicU64::new(0));
        (
            Self::with_notifier(status_change_tx, status_change_counter),
            status_change_rx,
        )
    }

    fn with_notifier(
        status_change_tx: watch::Sender<u64>,
        status_change_counter: Arc<AtomicU64>,
    ) -> Self {
        Self {
            status: Arc::new(Mutex::new(AgentStatus::default())),
            status_change_tx,
            status_change_counter,
        }
    }

    pub async fn update<F>(&self, update: F)
    where
        F: FnOnce(&mut AgentStatus),
    {
        let mut status = self.status.lock().await;
        update(&mut status);
        drop(status);

        let next = self.status_change_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.status_change_tx.send(next);
    }

    pub async fn snapshot(&self) -> AgentStatus {
        self.status.lock().await.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InitialAgentAlias {
    pub name: String,
    pub persistence: InitialAgentAliasPersistence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitialAgentAliasPersistence {
    User,
    GeneratedIfNoUserAlias,
}

pub(crate) struct ResolvedSpawnRequest {
    pub name: String,
    pub origin: AgentOrigin,
    pub custom_agent_id: Option<CustomAgentId>,
    pub parent_agent_id: Option<AgentId>,
    pub parent_session_id: Option<SessionId>,
    pub project_id: Option<ProjectId>,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub initial_input: Option<SendMessagePayload>,
    pub cost_hint: Option<SpawnCostHint>,
    pub session_settings: Option<SessionSettingsValues>,
    pub session_settings_schema: Option<SessionSettingsSchema>,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub resolved_spawn_config: ResolvedSpawnConfig,
    pub resume_session_id: Option<SessionId>,
    pub startup_warning: Option<String>,
    pub startup_failure: Option<String>,
    pub initial_alias: Option<InitialAgentAlias>,
    /// When true, all backend spawns use MockBackend regardless of backend_kind.
    /// Set by the test fixture.
    pub use_mock_backend: bool,
}

pub(crate) struct RelaySpawnRequest {
    pub name: String,
    pub origin: AgentOrigin,
    pub custom_agent_id: Option<CustomAgentId>,
    pub parent_agent_id: AgentId,
    pub project_id: Option<ProjectId>,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub session_id: SessionId,
}

pub(crate) struct SpawnedAgent {
    pub start: AgentStartPayload,
    pub handle: AgentHandle,
    pub startup_rx: oneshot::Receiver<Result<SessionId, String>>,
}

pub(crate) struct SpawnedRelayAgent {
    pub start: AgentStartPayload,
    pub handle: AgentHandle,
}

struct AgentEntry {
    handle: AgentHandle,
    status_handle: AgentStatusHandle,
}

impl AgentRegistry {
    pub fn new() -> Self {
        let (status_change_tx, _status_change_rx) = watch::channel(0);
        Self {
            agents: HashMap::new(),
            status_change_tx,
            status_change_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn spawn(
        &mut self,
        mut request: ResolvedSpawnRequest,
        session_store: Arc<Mutex<SessionStore>>,
        host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
        child_completion_tx: HostChildCompletionNoticeTx,
    ) -> SpawnedAgent {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        for server in &mut request.startup_mcp_servers {
            if server.name != "tyde-agent-control" {
                continue;
            }
            let StartupMcpTransport::Http { url, .. } = &mut server.transport else {
                panic!("tyde-agent-control MCP server must use HTTP transport");
            };
            *url = agent_control_mcp_url_for_agent(url, &agent_id);
        }
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            origin: request.origin,
            backend_kind: request.backend_kind,
            workspace_roots: request.workspace_roots.clone(),
            custom_agent_id: request.custom_agent_id.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: request.parent_agent_id.clone(),
            created_at_ms: now_ms(),
        };

        let status_handle = self.next_status_handle();
        let (handle, startup_rx) = spawn_agent_actor(
            agent_id.clone(),
            start.clone(),
            request,
            session_store,
            host_sub_agent_spawn_tx,
            child_completion_tx,
            status_handle.clone(),
        );

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle: handle.clone(),
                status_handle,
            },
        );
        assert!(
            previous.is_none(),
            "agent registry attempted to insert duplicate agent_id {}",
            agent_id
        );

        SpawnedAgent {
            start,
            handle,
            startup_rx,
        }
    }

    pub fn spawn_relay(
        &mut self,
        request: RelaySpawnRequest,
        events: mpsc::UnboundedReceiver<ChatEvent>,
        session_store: Arc<Mutex<SessionStore>>,
    ) -> SpawnedRelayAgent {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            origin: request.origin,
            backend_kind: request.backend_kind,
            workspace_roots: request.workspace_roots.clone(),
            custom_agent_id: request.custom_agent_id.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: Some(request.parent_agent_id.clone()),
            created_at_ms: now_ms(),
        };

        let status_handle = self.next_status_handle();
        let handle = spawn_relay_agent_actor(
            agent_id.clone(),
            start.clone(),
            events,
            session_store,
            request.session_id,
            status_handle.clone(),
        );

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle: handle.clone(),
                status_handle,
            },
        );
        assert!(
            previous.is_none(),
            "agent registry attempted to insert duplicate relay agent_id {}",
            agent_id
        );

        SpawnedRelayAgent { start, handle }
    }

    pub fn remove_agent(&mut self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.agents.remove(agent_id).map(|entry| entry.handle)
    }

    pub fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.agents.get(agent_id).map(|entry| entry.handle.clone())
    }

    pub fn agent_status_handle(&self, agent_id: &AgentId) -> Option<AgentStatusHandle> {
        self.agents
            .get(agent_id)
            .map(|entry| entry.status_handle.clone())
    }

    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }

    pub fn subscribe_status_changes(&self) -> watch::Receiver<u64> {
        self.status_change_tx.subscribe()
    }

    fn next_status_handle(&self) -> AgentStatusHandle {
        AgentStatusHandle::with_notifier(
            self.status_change_tx.clone(),
            Arc::clone(&self.status_change_counter),
        )
    }
}
