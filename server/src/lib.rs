mod acceptor;
mod agent;
pub(crate) mod agent_control_mcp;
pub mod backend;
pub(crate) mod browse_stream;
pub(crate) mod code_intel;
pub(crate) mod config_mcp;
pub(crate) mod connection;
pub(crate) mod debug_mcp;
pub(crate) mod error;
pub(crate) mod host;
pub(crate) mod mobile_access;
pub mod paths;
pub(crate) mod process_env;
pub(crate) mod project_stream;
pub mod remote;
pub(crate) mod review;
pub(crate) mod review_mcp;
pub(crate) mod router;
pub mod steering;
pub mod store;
pub(crate) mod stream;
pub(crate) mod sub_agent;
pub(crate) mod team_registry;
pub(crate) mod terminal_stream;
pub(crate) mod workflows;

pub use backend::{acp, antigravity, claude, codex, kiro, subprocess};

pub use acceptor::{HandshakeError, accept, listen_uds};
pub use agent_control_mcp::AgentControlMcpCaller;
pub use connection::{run_connection, run_mobile_connection};
#[cfg(feature = "test-support")]
pub use host::InstalledWorkbenchRemoveHook;
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

/// Process-global, exact host build version used to populate `release_version`
/// in the mobile Welcome/Reject/QR payloads (the web/PWA bundle key).
///
/// The `server` crate is unversioned (0.0.0); the real release version lives in
/// the host *binary* (`tyde-server`, `tauri-shell`), so each binary sets this
/// once at startup from its own `env!("CARGO_PKG_VERSION")` via
/// [`set_host_release_version`]. When unset (e.g. in unit tests), the payload
/// field is simply `None`, which is backward-compatible.
static HOST_RELEASE_VERSION: std::sync::OnceLock<protocol::TydeReleaseVersion> =
    std::sync::OnceLock::new();

/// Record the host's release version. Idempotent; a malformed value is ignored
/// (logged) rather than panicking the host. Call once at binary startup.
pub fn set_host_release_version(raw: &str) {
    match protocol::TydeReleaseVersion::parse(raw) {
        Ok(version) => {
            let _ = HOST_RELEASE_VERSION.set(version);
        }
        Err(error) => {
            tracing::warn!(raw, %error, "ignoring invalid host release version");
        }
    }
}

/// The host's release version, if a binary recorded one.
pub fn host_release_version() -> Option<protocol::TydeReleaseVersion> {
    HOST_RELEASE_VERSION.get().cloned()
}

/// Install the process-wide default rustls [`CryptoProvider`] (aws-lc-rs).
///
/// The server's `reqwest` client is built with the
/// `rustls-tls-webpki-roots-no-provider` feature, which deliberately does *not*
/// bundle a crypto provider: reqwest calls `CryptoProvider::get_default()` when
/// it constructs the client and panics with "No provider set" if none has been
/// installed. rustls itself only pulls in aws-lc-rs here (ring appears only for
/// webpki cert math, not as a provider), so install aws-lc-rs as the process
/// default *before* any TLS client — the mobile `reqwest` client or the MQTT
/// stack — is constructed.
///
/// Idempotent and safe to call from every entry point (binaries and tests): if
/// a provider is already installed, the redundant install is ignored.
pub fn install_default_crypto_provider() {
    use rustls::crypto::{CryptoProvider, aws_lc_rs};
    if CryptoProvider::get_default().is_none() {
        // `install_default` returns `Err` if another thread installed one first;
        // either way a provider is set afterward, so the result is ignored.
        let _ = aws_lc_rs::default_provider().install_default();
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
