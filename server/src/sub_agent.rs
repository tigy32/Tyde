use std::future::Future;
use std::pin::Pin;

use protocol::{AgentId, ChatEvent, SessionId};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(crate) struct SubAgentHandle {
    pub event_tx: mpsc::UnboundedSender<ChatEvent>,
}

pub(crate) trait SubAgentEmitter: Send + Sync {
    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<SessionId>,
    ) -> Pin<Box<dyn Future<Output = SubAgentHandle> + Send + '_>>;
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
    pub reply: oneshot::Sender<SubAgentHandle>,
}
