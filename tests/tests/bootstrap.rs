use std::time::Duration;

use client::ClientConfig;
use protocol::{
    BackendAccessMode, BackendKind, FrameKind, HostBootstrapPayload, HostBrowseInitial,
    HostBrowseStartPayload, HostSettingValue, NewAgentPayload, ProjectBootstrapPayload,
    ProjectRootPath, ReviewSummaryScope, SessionId, SessionSchemasPayload, SetSettingPayload,
    SpawnAgentParams, SpawnAgentPayload, TerminalCreatePayload, TerminalLaunchTarget,
};
use server::backend::BackendSession;
use server::store::project::ProjectStore;
use server::store::session::SessionStore;

async fn connect_raw(host: server::HostHandle) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake");
        if let Err(err) = server::run_connection(conn, host).await {
            eprintln!("server connection failed: {err:?}");
        }
    });

    client::connect(&ClientConfig::current(), client_stream)
        .await
        .expect("client handshake")
}

async fn next_env(client: &mut client::Connection, context: &str) -> protocol::Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn next_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> protocol::Envelope {
    loop {
        let env = next_env(client, context).await;
        if env.kind == kind {
            return env;
        }
    }
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    match tokio::time::timeout(duration, client.next_event()).await {
        Err(_) => {}
        Ok(Ok(None)) => {}
        Ok(Ok(Some(env))) => panic!(
            "unexpected event before {context}: kind={} stream={}",
            env.kind, env.stream
        ),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
    }
}

async fn expect_no_session_schemas(
    client: &mut client::Connection,
    duration: Duration,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        match tokio::time::timeout(deadline - now, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env))) if env.kind == FrameKind::SessionSchemas => {
                panic!(
                    "unexpected session_schemas before {context}: stream={} payload={}",
                    env.stream, env.payload
                );
            }
            Ok(Ok(Some(_))) => {}
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

fn spawn_host(dir: &tempfile::TempDir) -> server::HostHandle {
    server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host")
}

fn write_enabled_backends_settings(path: &std::path::Path, backends: &[BackendKind]) {
    let settings = protocol::HostSettings {
        enabled_backends: backends.to_vec(),
        default_backend: None,
        enable_mobile_connections: false,
        mobile_broker_url: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
    };
    let json = serde_json::json!({ "settings": settings });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json).expect("serialize settings"),
    )
    .expect("write settings");
}

#[tokio::test]
async fn connection_emits_one_host_bootstrap_without_old_initial_spam() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    assert_eq!(env.seq, 1, "Welcome consumes host seq 0");
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert!(bootstrap.sessions.is_empty());
    assert!(bootstrap.projects.is_empty());
    assert!(matches!(
        bootstrap.mobile_access.broker_status,
        protocol::MobileBrokerStatus::Disabled
    ));

    expect_no_event(
        &mut client,
        Duration::from_millis(100),
        "old initial replay spam",
    )
    .await;
}

#[tokio::test]
async fn stable_reconnect_does_not_emit_unchanged_session_schemas_after_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Kiro]);
    let missing_kiro = dir.path().join("missing-kiro-cli-chat");
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
        server::HostRuntimeConfig {
            kiro_probe_program: Some(missing_kiro.to_string_lossy().into_owned()),
            ..Default::default()
        },
    )
    .expect("spawn host");

    let mut first = connect_raw(host.clone()).await;
    let first_bootstrap = next_env(&mut first, "first host bootstrap").await;
    assert_eq!(first_bootstrap.kind, FrameKind::HostBootstrap);
    let first_live = next_kind(
        &mut first,
        FrameKind::SessionSchemas,
        "first Kiro schema refresh",
    )
    .await;
    let first_schemas: SessionSchemasPayload =
        first_live.parse_payload().expect("first SessionSchemas");
    assert!(
        matches!(
            first_schemas.schemas.first(),
            Some(protocol::SessionSchemaEntry::Unavailable { .. })
        ),
        "test expects the fake Kiro probe to settle to an unavailable schema"
    );

    let mut second = connect_raw(host).await;
    let second_bootstrap_env = next_env(&mut second, "second host bootstrap").await;
    assert_eq!(second_bootstrap_env.kind, FrameKind::HostBootstrap);
    let second_bootstrap: HostBootstrapPayload = second_bootstrap_env
        .parse_payload()
        .expect("second HostBootstrap");
    assert_eq!(second_bootstrap.session_schemas, first_schemas.schemas);

    expect_no_session_schemas(
        &mut second,
        Duration::from_millis(500),
        "stable reconnect duplicate schema replay",
    )
    .await;
}

#[tokio::test]
async fn changed_session_schemas_still_emit_live_after_host_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Claude]);
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let bootstrap_env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(bootstrap_env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = bootstrap_env.parse_payload().expect("HostBootstrap");
    assert_eq!(bootstrap.session_schemas.len(), 1);
    assert_eq!(
        bootstrap.session_schemas[0].backend_kind(),
        BackendKind::Claude
    );

    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
            },
        })
        .await
        .expect("set enabled backends");

    let schemas_env = next_kind(
        &mut client,
        FrameKind::SessionSchemas,
        "changed session schemas",
    )
    .await;
    let schemas: SessionSchemasPayload =
        schemas_env.parse_payload().expect("SessionSchemas payload");
    assert_eq!(
        schemas
            .schemas
            .iter()
            .map(protocol::SessionSchemaEntry::backend_kind)
            .collect::<Vec<_>>(),
        vec![BackendKind::Claude, BackendKind::Codex]
    );
}

#[tokio::test]
async fn host_bootstrap_includes_session_summaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    let store = SessionStore::load(session_path.clone()).expect("load session store");
    store
        .upsert_backend_session(
            &BackendSession {
                id: SessionId("session-1".to_owned()),
                backend_kind: BackendKind::Claude,
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                title: Some("Existing session".to_owned()),
                token_count: Some(42),
                created_at_ms: Some(10),
                updated_at_ms: Some(20),
                resumable: true,
            },
            None,
            None,
            None,
        )
        .expect("insert session");

    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(bootstrap.sessions.len(), 1);
    assert_eq!(bootstrap.sessions[0].id.0, "session-1");
    assert_eq!(
        bootstrap.sessions[0].alias.as_deref(),
        Some("Existing session")
    );
    assert_eq!(bootstrap.sessions[0].token_count, Some(42));
}

#[tokio::test]
async fn project_subscription_starts_with_project_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = tempfile::tempdir().expect("project root");
    let project_path = dir.path().join("projects.json");
    let project = ProjectStore::load(project_path.clone())
        .expect("load project store")
        .create(
            "Existing project".to_owned(),
            vec![ProjectRootPath(root.path().to_string_lossy().to_string())],
        )
        .expect("create project");

    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        project_path,
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    let host_bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(host_bootstrap.projects.len(), 1);
    assert_eq!(host_bootstrap.projects[0].id, project.id);

    let env = next_env(&mut client, "project bootstrap").await;
    assert_eq!(env.kind, FrameKind::ProjectBootstrap);
    assert_eq!(env.stream.0, format!("/project/{}", project.id.0));
    assert_eq!(env.seq, 0);
    let bootstrap: ProjectBootstrapPayload =
        env.parse_payload().expect("project bootstrap payload");
    assert_eq!(bootstrap.project.id, project.id);
    assert_eq!(bootstrap.review_summaries.len(), 1);
    assert_eq!(
        bootstrap.review_summaries[0].scope,
        ReviewSummaryScope::Workspace
    );
    assert!(matches!(
        bootstrap.review_summaries[0].status,
        protocol::ReviewStatus::Draft
    ));
}

#[tokio::test]
async fn live_agent_reconnect_starts_with_agent_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut first = connect_raw(host.clone()).await;
    let _ = next_env(&mut first, "initial host bootstrap").await;

    first
        .spawn_agent(SpawnAgentPayload {
            name: Some("Bootstrap Agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");

    let new_agent_env = next_kind(&mut first, FrameKind::NewAgent, "new agent").await;
    let new_agent: NewAgentPayload = new_agent_env.parse_payload().expect("new agent payload");
    loop {
        let env = next_env(&mut first, "agent start replay").await;
        match env.kind {
            FrameKind::AgentBootstrap => {
                let bootstrap: protocol::AgentBootstrapPayload =
                    env.parse_payload().expect("agent bootstrap payload");
                if bootstrap
                    .events
                    .iter()
                    .any(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_)))
                {
                    break;
                }
            }
            FrameKind::AgentStart => break,
            _ => {}
        }
    }

    let mut second = connect_raw(host).await;
    let env = next_env(&mut second, "host bootstrap").await;
    let host_bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    let bootstrapped_agent = host_bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .expect("live agent in HostBootstrap");

    let env = loop {
        let env = next_env(&mut second, "agent bootstrap").await;
        if env.stream == bootstrapped_agent.instance_stream {
            break env;
        }
    };
    assert_eq!(env.kind, FrameKind::AgentBootstrap);
    assert_eq!(env.seq, 0);
    let bootstrap: protocol::AgentBootstrapPayload =
        env.parse_payload().expect("agent bootstrap payload");
    assert!(
        bootstrap
            .events
            .iter()
            .any(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_))),
        "AgentBootstrap should carry the replayed AgentStart"
    );
}

#[tokio::test]
async fn browse_and_terminal_streams_start_with_bootstraps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut client = connect_raw(host).await;
    let _ = next_env(&mut client, "host bootstrap").await;

    let browse_stream = protocol::StreamPath(format!("/browse/{}", uuid::Uuid::new_v4()));
    client
        .host_browse_start(HostBrowseStartPayload {
            browse_stream: browse_stream.clone(),
            initial: HostBrowseInitial::Path {
                path: protocol::HostAbsPath(dir.path().to_string_lossy().to_string()),
            },
            include_hidden: false,
        })
        .await
        .expect("start browse");
    let browse = next_env(&mut client, "browse bootstrap").await;
    assert_eq!(browse.kind, FrameKind::BrowseBootstrap);
    assert_eq!(browse.stream, browse_stream);
    assert_eq!(browse.seq, 0);

    client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::HostDefault,
            cols: 80,
            rows: 24,
        })
        .await
        .expect("create terminal");
    let terminal = next_kind(&mut client, FrameKind::NewTerminal, "new terminal").await;
    let new_terminal: protocol::NewTerminalPayload =
        terminal.parse_payload().expect("new terminal");
    let terminal_bootstrap = next_env(&mut client, "terminal bootstrap").await;
    assert_eq!(terminal_bootstrap.kind, FrameKind::TerminalBootstrap);
    assert_eq!(terminal_bootstrap.stream, new_terminal.stream);
    assert_eq!(terminal_bootstrap.seq, 0);
}
