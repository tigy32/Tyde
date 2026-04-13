pub mod acp;
pub mod admin;
pub mod agent;
pub mod agent_defs_io;
pub mod backends;
pub mod chat_buffer;
pub mod conversation_sessions;
pub mod debug_log;
pub mod dependencies;
pub mod file_service;
pub mod file_watch;
pub mod git_service;
pub mod invoke;
pub mod mock_backend;
pub mod remote_control;
pub mod runtime_ops;
pub mod server_state;
pub mod skill_injection;
pub mod steering;
pub mod stores;
pub mod terminal;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tool_policy;
pub mod usage;
pub mod workflow_io;

pub type AgentId = String;

pub use tool_policy::ToolPolicy;

/// Backward-compatibility re-export. New code should use `agent::` directly.
pub mod agent_runtime {
    pub use crate::agent::{
        AgentEvent, AgentEventBatch, AgentInfo, AgentRegistry, CollectedAgentResult,
    };
}
