mod acceptor;
mod agent;
pub(crate) mod agent_control_mcp;
pub mod backend;
pub(crate) mod browse_stream;
pub(crate) mod connection;
pub(crate) mod debug_mcp;
pub(crate) mod host;
pub(crate) mod process_env;
pub(crate) mod project_stream;
pub mod remote;
pub(crate) mod router;
pub mod steering;
pub mod store;
pub(crate) mod stream;
pub(crate) mod sub_agent;
pub(crate) mod terminal_stream;

pub use backend::{acp, claude, codex, gemini, kiro, subprocess};

pub use acceptor::{HandshakeError, accept, listen_uds};
pub use connection::run_connection;
pub use host::{
    HostHandle, HostRuntimeConfig, spawn_host, spawn_host_with_mock_backend,
    spawn_host_with_mock_backend_and_runtime_config, spawn_host_with_session_store,
    spawn_host_with_store_paths, spawn_host_with_store_paths_and_runtime_config,
};

use std::collections::HashMap;

use protocol::{PROTOCOL_VERSION, SeqValidator, StreamPath, TYDE_VERSION, Version};
use tokio::io::{AsyncBufRead, AsyncWrite};

#[derive(Clone, Copy)]
pub struct ServerConfig {
    pub protocol_version: u32,
    pub tyde_version: Version,
}

impl ServerConfig {
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
        }
    }
}

pub struct Connection {
    pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub incoming_seq: SeqValidator,
    pub outgoing_seq: HashMap<StreamPath, u64>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("incoming_seq", &self.incoming_seq)
            .field("outgoing_seq", &self.outgoing_seq)
            .finish_non_exhaustive()
    }
}
