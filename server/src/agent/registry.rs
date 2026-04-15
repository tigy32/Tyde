use std::collections::HashMap;
use std::sync::Arc;

use protocol::{
    AgentId, AgentStartPayload, BackendKind, ProjectId, SendMessagePayload, SessionId,
    SpawnCostHint,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::agent::{AgentHandle, now_ms, spawn_agent_actor};
use crate::store::session::SessionStore;

pub(crate) struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
}

pub(crate) struct ResolvedSpawnRequest {
    pub name: String,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub initial_input: Option<SendMessagePayload>,
    pub cost_hint: Option<SpawnCostHint>,
    pub resume_session_id: Option<SessionId>,
    /// When true, all backend spawns use MockBackend regardless of backend_kind.
    /// Set by the test fixture.
    pub use_mock_backend: bool,
}

struct AgentEntry {
    handle: AgentHandle,
    start: AgentStartPayload,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    pub async fn spawn(
        &mut self,
        request: ResolvedSpawnRequest,
        session_store: Arc<Mutex<SessionStore>>,
    ) -> Result<(AgentStartPayload, SessionId), String> {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            backend_kind: request.backend_kind,
            workspace_roots: request.workspace_roots.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: request.parent_agent_id.clone(),
            created_at_ms: now_ms(),
        };

        let (handle, session_id) =
            spawn_agent_actor(agent_id.clone(), start.clone(), request, session_store).await?;

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle,
                start: start.clone(),
            },
        );
        assert!(
            previous.is_none(),
            "agent registry attempted to insert duplicate agent_id {}",
            agent_id
        );

        Ok((start, session_id))
    }

    pub fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.agents.get(agent_id).map(|entry| entry.handle.clone())
    }

    pub fn list_agents(&self) -> Vec<AgentStartPayload> {
        self.agents
            .values()
            .map(|entry| entry.start.clone())
            .collect()
    }
}
