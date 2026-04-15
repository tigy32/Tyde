use std::path::PathBuf;

use protocol::FrameKind;
use tyde_dev_driver::agent_control::AgentControlHandle;

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

pub struct Fixture {
    pub client: client::Connection,
    #[allow(dead_code)]
    host: server::HostHandle,
    #[allow(dead_code)]
    session_store_dir: tempfile::TempDir,
}

impl Fixture {
    pub async fn new() -> Self {
        init_tracing();

        let session_store_dir = tempfile::tempdir().expect("create session tempdir");
        let session_path = session_store_dir.path().join("sessions.json");
        let project_path = session_store_dir.path().join("projects.json");
        let settings_path = session_store_dir.path().join("settings.json");
        let host = server::spawn_host_with_mock_backend(session_path, project_path, settings_path)
            .expect("initialize host with mock backend");
        let client = connect_client(host.clone()).await;

        Self {
            client,
            host,
            session_store_dir,
        }
    }

    #[allow(dead_code)]
    pub async fn connect(&self) -> client::Connection {
        connect_client(self.host.clone()).await
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
        let host = server::spawn_host_with_mock_backend(
            self.session_store_path(),
            self.project_store_path(),
            self.settings_store_path(),
        )
        .expect("initialize fresh host with existing stores");
        connect_client(host).await
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
}

async fn connect_client(host: server::HostHandle) -> client::Connection {
    let mut client = connect_raw_client(host).await;

    let env = client
        .next_event()
        .await
        .expect("initial host settings read failed")
        .expect("connection closed before initial host settings");
    assert_eq!(
        env.kind,
        FrameKind::HostSettings,
        "first host event on connect must be HostSettings"
    );

    client
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
