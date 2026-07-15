use std::future::Future;
use std::pin::Pin;

use protocol::{AgentId, BackendCapacityState, BackendKind, ChatEvent, SessionId};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(crate) struct SubAgentHandle {
    pub event_tx: mpsc::UnboundedSender<ChatEvent>,
    /// Id of the spawned sub-agent, included in `ToolProgress` updates
    /// on the parent's Task tool card so the frontend can link to the
    /// sub-agent's own view.
    pub agent_id: AgentId,
}

pub(crate) trait SubAgentEmitter: Send + Sync {
    fn on_backend_capacity(&self, _backend_kind: BackendKind, _state: BackendCapacityState) {}
    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<SessionId>,
    ) -> Pin<Box<dyn Future<Output = Result<SubAgentHandle, String>> + Send + '_>>;
}

pub(crate) type HostSubAgentSpawnTx = mpsc::UnboundedSender<HostSubAgentSpawnRequest>;
pub(crate) type HostSubAgentSpawnRx = mpsc::UnboundedReceiver<HostSubAgentSpawnRequest>;

pub(crate) struct HostSubAgentSpawnRequest {
    pub parent_agent_id: AgentId,
    pub workspace_roots: Vec<String>,
    pub tool_use_id: String,
    pub name: String,
    pub description: String,
    pub agent_type: String,
    pub session_id_hint: Option<SessionId>,
    pub reply: oneshot::Sender<Result<SubAgentHandle, String>>,
}
