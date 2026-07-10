use std::path::{Path, PathBuf};

use protocol::{AgentId, CustomAgentNotifyPayload, Envelope, FrameKind, HostBootstrapPayload};
use tyde_dev_driver::agent_control::AgentControlHandle;

// `cargo` compiles each integration test file as its own binary, so a
// helper used in only some of them looks dead in the others. The
// `#[allow(dead_code)]` covers that — every test file that needs to
// skip the host's built-in team CustomAgent replay calls
// `is_builtin_team_custom_agent_notify`.
#[allow(dead_code)]
const BUILTIN_TEAM_CUSTOM_AGENT_IDS: &[&str] = &[
    "tyde-team-lead",
    "tyde-code-reviewer",
    "tyde-frontend-engineer",
    "tyde-backend-engineer",
    "tyde-test-qa-engineer",
    "tyde-debugger",
];

#[allow(dead_code)]
pub fn is_builtin_team_custom_agent_notify(env: &Envelope) -> bool {
    if env.kind != FrameKind::CustomAgentNotify {
        return false;
    }
    let payload = env
        .parse_payload::<CustomAgentNotifyPayload>()
        .expect("parse CustomAgentNotifyPayload while checking built-in team custom agent");
    match payload {
        CustomAgentNotifyPayload::Upsert { custom_agent } => {
            BUILTIN_TEAM_CUSTOM_AGENT_IDS.contains(&custom_agent.id.0.as_str())
        }
        CustomAgentNotifyPayload::Delete { .. } => false,
    }
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

pub struct Fixture {
    pub client: client::Connection,
    #[allow(dead_code)]
    pub bootstrap: HostBootstrapPayload,
    #[allow(dead_code)]
    host: server::HostHandle,
    #[allow(dead_code)]
    session_store_dir: tempfile::TempDir,
    antigravity_conversations_dir: tempfile::TempDir,
}

impl Fixture {
    #[allow(dead_code)]
    pub async fn new() -> Self {
        Self::new_with_runtime_config(server::HostRuntimeConfig::default()).await
    }

    /// Like [`Fixture::new`] but actually probes the real backend CLIs
    /// (`<cli> --version`, codex model discovery, etc.). Spawning real
    /// subprocesses costs several seconds per fixture, so only the handful of
    /// tests asserting on backend-setup *contents* should use this — everyone
    /// else gets the fast stub via `new`/`new_with_runtime_config`.
    #[allow(dead_code)]
    pub async fn new_with_real_backend_probe() -> Self {
        Self::new_with_runtime_config_inner(server::HostRuntimeConfig::default(), false).await
    }

    #[allow(dead_code)]
    pub async fn new_with_runtime_config(runtime_config: server::HostRuntimeConfig) -> Self {
        Self::new_with_runtime_config_inner(runtime_config, true).await
    }

    async fn new_with_runtime_config_inner(
        mut runtime_config: server::HostRuntimeConfig,
        skip_real_backend_probe: bool,
    ) -> Self {
        init_tracing();

        // Real backend probing spawns `<cli> --version` for every backend and
        // runs codex model discovery (a network RPC) on every host spawn —
        // several seconds each, paid once per fixture. The default test
        // fixture skips it so the suite stays fast; tests that assert on probe
        // output opt back in via `new_with_real_backend_probe`.
        runtime_config.skip_real_backend_probe = skip_real_backend_probe;

        let antigravity_conversations_dir =
            tempfile::tempdir().expect("create Antigravity conversations tempdir");
        runtime_config.antigravity_conversations_dir =
            Some(antigravity_conversations_dir.path().to_path_buf());
        let session_store_dir = tempfile::tempdir().expect("create session tempdir");
        let session_path = session_store_dir.path().join("sessions.json");
        let project_path = session_store_dir.path().join("projects.json");
        let settings_path = session_store_dir.path().join("settings.json");
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            session_path,
            project_path,
            settings_path,
            runtime_config,
        )
        .expect("initialize host with mock backend");
        let (client, bootstrap) = connect_client_with_bootstrap(host.clone()).await;

        Self {
            client,
            bootstrap,
            host,
            session_store_dir,
            antigravity_conversations_dir,
        }
    }

    #[allow(dead_code)]
    pub async fn connect(&self) -> client::Connection {
        connect_client(self.host.clone()).await
    }

    #[allow(dead_code)]
    pub async fn connect_with_bootstrap(&self) -> (client::Connection, HostBootstrapPayload) {
        connect_client_with_bootstrap(self.host.clone()).await
    }

    #[allow(dead_code)]
    pub async fn connect_agent_control(&self) -> AgentControlHandle {
        let client = connect_raw_client(self.host.clone()).await;
        AgentControlHandle::from_connection(client)
            .await
            .expect("agent-control connection should bootstrap")
    }

    #[allow(dead_code)]
    pub async fn connect_fresh_host(&self) -> client::Connection {
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            self.session_store_path(),
            self.project_store_path(),
            self.settings_store_path(),
            self.fresh_host_runtime_config(),
        )
        .expect("initialize fresh host with existing stores");
        connect_client(host).await
    }

    #[allow(dead_code)]
    pub async fn connect_fresh_host_with_bootstrap(
        &self,
    ) -> (client::Connection, HostBootstrapPayload) {
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            self.session_store_path(),
            self.project_store_path(),
            self.settings_store_path(),
            self.fresh_host_runtime_config(),
        )
        .expect("initialize fresh host with existing stores");
        connect_client_with_bootstrap(host).await
    }

    #[allow(dead_code)]
    pub async fn agent_ids(&self) -> Vec<AgentId> {
        self.host.agent_ids().await
    }

    #[allow(dead_code)]
    pub async fn agent_control_http_url(&self) -> String {
        self.host.agent_control_mcp_url().await
    }

    #[allow(dead_code)]
    pub async fn review_mcp_http_url(&self) -> String {
        self.host.review_mcp_url().await
    }

    #[allow(dead_code)]
    pub async fn workflow_mcp_http_url(&self) -> String {
        self.host.workflow_mcp_url().await
    }

    fn session_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("sessions.json")
    }

    fn project_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("projects.json")
    }

    fn settings_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("settings.json")
    }

    fn fresh_host_runtime_config(&self) -> server::HostRuntimeConfig {
        server::HostRuntimeConfig {
            antigravity_conversations_dir: Some(
                self.antigravity_conversations_dir.path().to_path_buf(),
            ),
            skip_real_backend_probe: true,
            ..server::HostRuntimeConfig::default()
        }
    }

    #[allow(dead_code)]
    pub fn store_dir(&self) -> &Path {
        self.session_store_dir.path()
    }

    #[allow(dead_code)]
    pub fn antigravity_conversations_dir(&self) -> &Path {
        self.antigravity_conversations_dir.path()
    }
}

async fn connect_client(host: server::HostHandle) -> client::Connection {
    connect_client_with_bootstrap(host).await.0
}

async fn connect_client_with_bootstrap(
    host: server::HostHandle,
) -> (client::Connection, HostBootstrapPayload) {
    let mut client = connect_raw_client(host).await;

    let env = client
        .next_event()
        .await
        .expect("initial host bootstrap read failed")
        .expect("connection closed before initial host bootstrap");
    assert_eq!(
        env.kind,
        FrameKind::HostBootstrap,
        "first host event on connect must be HostBootstrap"
    );
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse HostBootstrapPayload");

    (client, bootstrap)
}

async fn connect_raw_client(host: server::HostHandle) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig::current();

    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake failed");
        if let Err(err) = server::run_connection(conn, host).await {
            eprintln!("server connection loop failed: {err:?}");
        }
    });

    client::connect(&client_config, client_stream)
        .await
        .expect("client handshake failed")
}
