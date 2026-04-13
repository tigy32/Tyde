mod registry;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backends::types::SessionCommand;
use crate::{AgentId, ToolPolicy};

pub use crate::backends::Backend;
pub use registry::AgentRegistry;

// ── Types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: AgentId,
    pub ui_owner_project_id: Option<String>,
    pub workspace_roots: Vec<String>,
    pub backend_kind: String,
    pub parent_agent_id: Option<AgentId>,
    pub name: String,
    pub agent_type: Option<String>,
    pub agent_definition_id: Option<String>,
    #[serde(skip)]
    pub tool_policy: ToolPolicy,
    pub is_running: bool,
    pub summary: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub agent_id: AgentId,
    pub kind: String,
    pub is_running: bool,
    pub timestamp_ms: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentEventBatch {
    pub events: Vec<AgentEvent>,
    pub latest_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedAgentResult {
    pub agent: AgentInfo,
    pub final_message: Option<String>,
    pub changed_files: Vec<String>,
    pub tool_results: Vec<Value>,
}

// ── AgentHandle ─────────────────────────────────────────────────────

/// Cloneable, type-erased handle for sending commands to an agent's
/// backend. Obtained from the registry, used after releasing the lock.
#[derive(Clone)]
pub struct AgentHandle {
    inner: Arc<dyn CommandExecutor>,
}

impl AgentHandle {
    pub fn new(executor: Arc<dyn CommandExecutor>) -> Self {
        Self { inner: executor }
    }

    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }
}

/// Object-safe trait for executing commands. Implementors are the
/// concrete command handle types (e.g. ClaudeCommandHandle,
/// MockCommandHandle).
pub trait CommandExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

// ── Agent ───────────────────────────────────────────────────────────

pub struct Agent {
    pub info: AgentInfo,
    backend: Option<Box<dyn Backend>>,
}

impl Agent {
    pub fn agent_handle(&self) -> Option<AgentHandle> {
        self.backend.as_ref().map(|b| b.agent_handle())
    }

    pub fn tracks_local_session_store(&self) -> bool {
        self.backend
            .as_ref()
            .map_or(false, |b| b.tracks_local_session_store())
    }

    pub fn take_backend(&mut self) -> Option<Box<dyn Backend>> {
        self.backend.take()
    }

    pub fn backend(&self) -> Option<&dyn Backend> {
        self.backend.as_deref()
    }
}
