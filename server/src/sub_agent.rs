use std::future::Future;
use std::pin::Pin;

use protocol::{
    AgentId, BackendCapacityState, BackendKind, ChatEvent, ModelRequestTokenUsage, SessionId,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(crate) struct SubAgentHandle {
    pub event_tx: mpsc::UnboundedSender<ChatEvent>,
    /// Accounting stays backend-only so authoritative per-request usage never
    /// becomes a public ChatEvent merely to cross the native-child relay.
    pub model_usage_tx: mpsc::UnboundedSender<ModelRequestTokenUsage>,
    /// Aggregate-only accounting from providers that do not expose a truthful
    /// input/output split for native children.
    pub total_usage_tx: mpsc::UnboundedSender<u64>,
    /// Id of the spawned sub-agent, included in `ToolProgress` updates
    /// on the parent's Task tool card so the frontend can link to the
    /// sub-agent's own view.
    pub agent_id: AgentId,
    /// Backend observations can arrive out of order. Later authoritative
    /// metadata may replace an initial generic name through this channel.
    /// Host applies it as a generated alias, so an explicit user rename wins.
    pub name_update_tx: Option<mpsc::UnboundedSender<String>>,
}

pub(crate) fn child_name_is_better(current: &str, candidate: &str) -> bool {
    child_name_quality(candidate) > child_name_quality(current)
}

fn child_name_quality(name: &str) -> u8 {
    let trimmed = name.trim();
    let normalized = trimmed.to_ascii_lowercase().replace(['-', '_', '/'], " ");
    if normalized.is_empty() {
        return 0;
    }
    if trimmed.starts_with('/') {
        return 1;
    }
    if matches!(
        normalized.as_str(),
        "agent" | "child" | "child agent" | "sub agent" | "task" | "spawnagent" | "general purpose"
    ) {
        return 1;
    }
    2
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
