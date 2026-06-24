use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{
    AgentControlStatus, AgentErrorCode, AgentId, AgentOrigin, AgentStartPayload,
    AgentWorkflowMetadata, BackendAccessMode, BackendKind, ChatEvent, CustomAgentId, ProjectId,
    SendMessagePayload, SessionId, SessionSettingsSchema, SessionSettingsValues, SpawnCostHint,
    TeamId, TeamMemberId,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::agent::customization::ResolvedSpawnConfig;
use crate::agent::{AgentHandle, now_ms, spawn_agent_actor, spawn_relay_agent_actor};
use crate::backend::StartupMcpServer;
use crate::backend::StartupMcpTransport;
use crate::host::mcp_url_for_agent;
use crate::review::ReviewRegistryHandle;
use crate::review_mcp::REVIEW_FEEDBACK_MCP_SERVER_NAME;
use crate::store::session::SessionStore;
use crate::sub_agent::HostSubAgentSpawnTx;
use crate::workflows::mcp::WORKFLOW_PROGRESS_MCP_SERVER_NAME;

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
    pub pending_user_response: Option<PendingUserResponseKind>,
    pub last_error: Option<String>,
    pub activity_counter: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingUserResponseKind {
    PlanApproval,
}

impl AgentStatus {
    pub fn is_active(&self) -> bool {
        !self.terminated && (!self.started || self.is_thinking || !self.turn_completed)
    }

    pub fn is_plan_approval_pending(&self) -> bool {
        self.pending_user_response.is_some()
    }

    pub fn status(&self) -> AgentControlStatus {
        if self.terminated && self.last_error.is_some() {
            AgentControlStatus::Failed
        } else if self.is_plan_approval_pending() {
            AgentControlStatus::Idle
        } else if self.is_active() {
            AgentControlStatus::Thinking
        } else {
            AgentControlStatus::Idle
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
    pub team_id: Option<TeamId>,
    pub team_member_id: Option<TeamMemberId>,
    pub workflow: Option<AgentWorkflowMetadata>,
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
    pub fork_from_session_id: Option<SessionId>,
    pub startup_warning: Option<String>,
    pub startup_failure: Option<AgentStartupFailure>,
    pub initial_alias: Option<InitialAgentAlias>,
    /// When true, all backend spawns use MockBackend regardless of backend_kind.
    /// Set by the test fixture.
    pub use_mock_backend: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentStartupFailure {
    pub code: AgentErrorCode,
    pub message: String,
}

impl AgentStartupFailure {
    pub fn backend_failed(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::BackendFailed,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::Internal,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::Unsupported,
            message: message.into(),
        }
    }
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
    pub workflow: Option<AgentWorkflowMetadata>,
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
    access_mode: BackendAccessMode,
    parent_agent_id: Option<AgentId>,
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
        review_registry: ReviewRegistryHandle,
    ) -> SpawnedAgent {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        for server in &mut request.startup_mcp_servers {
            if server.name != "tyde-agent-control"
                && server.name != REVIEW_FEEDBACK_MCP_SERVER_NAME
                && server.name != WORKFLOW_PROGRESS_MCP_SERVER_NAME
            {
                continue;
            }
            let StartupMcpTransport::Http { url, .. } = &mut server.transport else {
                panic!("Tyde injected MCP servers must use HTTP transport");
            };
            *url = mcp_url_for_agent(url, &agent_id);
        }
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            origin: request.origin,
            backend_kind: request.backend_kind,
            workspace_roots: request.workspace_roots.clone(),
            custom_agent_id: request.custom_agent_id.clone(),
            team_id: request.team_id.clone(),
            team_member_id: request.team_member_id.clone(),
            workflow: request.workflow.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: request.parent_agent_id.clone(),
            session_id: request.resume_session_id.clone(),
            created_at_ms: now_ms(),
        };

        let access_mode = request.resolved_spawn_config.access_mode;
        let status_handle = self.next_status_handle();
        let (handle, startup_rx) = spawn_agent_actor(
            agent_id.clone(),
            start.clone(),
            request,
            session_store,
            host_sub_agent_spawn_tx,
            review_registry,
            status_handle.clone(),
        );

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle: handle.clone(),
                status_handle,
                access_mode,
                parent_agent_id: start.parent_agent_id.clone(),
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
            team_id: None,
            team_member_id: None,
            workflow: request.workflow.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: Some(request.parent_agent_id.clone()),
            session_id: Some(request.session_id.clone()),
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
                access_mode: BackendAccessMode::Unrestricted,
                parent_agent_id: start.parent_agent_id.clone(),
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

    pub fn agent_access_mode(&self, agent_id: &AgentId) -> Option<BackendAccessMode> {
        self.agents.get(agent_id).map(|entry| entry.access_mode)
    }

    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }

    pub fn agent_subtree_post_order(&self, agent_id: &AgentId) -> Vec<(AgentId, AgentHandle)> {
        if !self.agents.contains_key(agent_id) {
            return Vec::new();
        }

        let mut children_by_parent: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
        for (candidate_id, entry) in &self.agents {
            if let Some(parent_agent_id) = &entry.parent_agent_id {
                children_by_parent
                    .entry(parent_agent_id.clone())
                    .or_default()
                    .push(candidate_id.clone());
            }
        }
        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| left.0.cmp(&right.0));
        }

        let mut visited = HashSet::new();
        let mut ordered = Vec::new();
        collect_agent_subtree_post_order(
            agent_id,
            &self.agents,
            &children_by_parent,
            &mut visited,
            &mut ordered,
        );
        ordered
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

fn collect_agent_subtree_post_order(
    agent_id: &AgentId,
    agents: &HashMap<AgentId, AgentEntry>,
    children_by_parent: &HashMap<AgentId, Vec<AgentId>>,
    visited: &mut HashSet<AgentId>,
    ordered: &mut Vec<(AgentId, AgentHandle)>,
) {
    if !visited.insert(agent_id.clone()) {
        return;
    }

    if let Some(children) = children_by_parent.get(agent_id) {
        for child_id in children {
            collect_agent_subtree_post_order(
                child_id,
                agents,
                children_by_parent,
                visited,
                ordered,
            );
        }
    }

    if let Some(entry) = agents.get(agent_id) {
        ordered.push((agent_id.clone(), entry.handle.clone()));
    }
}
