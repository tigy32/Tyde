use std::path::PathBuf;
use std::time::Duration;

use client::{AgentEndpoint, AgentEvent, HostEndpoint, HostEvent, ProjectEvent};
use protocol::{
    AgentBootstrapEvent, BackendKind, ChatEvent, HostSettingValue, ProjectRootPath,
    ReviewSummaryScope, SendMessagePayload, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

#[derive(Debug)]
enum AgentProbe {
    Started(String),
    Final(String),
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

#[tokio::test]
async fn runtime_accepts_live_backend_config_schemas() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        session_store_dir.path().join("projects.json"),
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let HostEndpoint {
        mut events,
        commands: _,
    } = connect_runtime(host.clone()).await;
    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(_) => {}
        _ => panic!("expected initial HostBootstrap"),
    }

    let mut raw = connect_raw(host).await;
    let bootstrap = raw
        .next_event()
        .await
        .expect("raw bootstrap read failed")
        .expect("raw connection closed before bootstrap");
    assert_eq!(bootstrap.kind, protocol::FrameKind::HostBootstrap);
    raw.set_setting(SetSettingPayload {
        setting: HostSettingValue::EnabledBackends {
            enabled_backends: vec![BackendKind::Hermes],
        },
    })
    .await
    .expect("enable Hermes");

    loop {
        match next_host_event(&mut events, "live BackendConfigSchemas").await {
            HostEvent::BackendConfigSchemas(payload) => {
                assert!(
                    payload
                        .schemas
                        .iter()
                        .any(|schema| schema.backend_kind == BackendKind::Hermes),
                    "BackendConfigSchemas should include Hermes: {payload:?}"
                );
                break;
            }
            HostEvent::HostSettings(_)
            | HostEvent::SessionSchemas(_)
            | HostEvent::LaunchProfileCatalogNotify(_)
            | HostEvent::BackendSetup(_)
            | HostEvent::BackendConfigSnapshots(_)
            | HostEvent::AgentsViewPreferencesNotify(_)
            | HostEvent::TeamPresetCatalogNotify(_) => {}
            _ => panic!("unexpected host event while waiting for BackendConfigSchemas"),
        }
    }
}

#[tokio::test]
async fn split_endpoints_allow_event_loops_and_commands_to_run_independently() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        session_store_dir.path().join("projects.json"),
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let host_endpoint = connect_runtime(host).await;
    let HostEndpoint {
        mut events,
        commands,
    } = host_endpoint;

    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(payload) => {
            assert!(payload.sessions.is_empty());
            assert!(payload.projects.is_empty());
        }
        _ => panic!("expected initial HostBootstrap"),
    }

    let (session_list_tx, session_list_rx) = oneshot::channel();
    let (new_agent_tx, new_agent_rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut session_list_tx = Some(session_list_tx);
        let mut new_agent_tx = Some(new_agent_tx);

        while let Some(event) = events.recv().await {
            match event {
                HostEvent::SessionList(payload) => {
                    if let Some(tx) = session_list_tx.take() {
                        let _ = tx.send(payload);
                    }
                }
                HostEvent::NewAgent(agent) => {
                    if let Some(tx) = new_agent_tx.take() {
                        let _ = tx.send(agent);
                    }
                }
                HostEvent::HostSettings(_)
                | HostEvent::AgentActivitySummary(_)
                | HostEvent::TaskTokenUsage(_)
                | HostEvent::AgentsViewPreferencesNotify(_)
                | HostEvent::HostBootstrap(_)
                | HostEvent::BackendSetup(_)
                | HostEvent::BackendConfigSchemas(_)
                | HostEvent::BackendConfigSnapshots(_)
                | HostEvent::AgentClosed(_)
                | HostEvent::ProjectNotify(_)
                | HostEvent::NewTerminal(_)
                | HostEvent::SessionSchemas(_)
                | HostEvent::LaunchProfileCatalogNotify(_)
                | HostEvent::CommandError(_)
                | HostEvent::CustomAgentNotify(_)
                | HostEvent::SteeringNotify(_)
                | HostEvent::SkillNotify(_)
                | HostEvent::McpServerNotify(_)
                | HostEvent::WorkflowNotify(_)
                | HostEvent::WorkflowRunNotify(_)
                | HostEvent::MobileAccessState(_)
                | HostEvent::MobilePairingOffer(_)
                | HostEvent::TeamNotify(_)
                | HostEvent::TeamMemberNotify(_)
                | HostEvent::TeamMemberBindingNotify(_)
                | HostEvent::TeamPresetCatalogNotify(_)
                | HostEvent::TeamDraftNotify(_)
                | HostEvent::TeamMemberShuffleSuggestionNotify(_) => {}
            }

            if session_list_tx.is_none() && new_agent_tx.is_none() {
                break;
            }
        }
    });

    commands
        .list_sessions()
        .await
        .expect("list_sessions command should succeed");
    let sessions = timeout(Duration::from_secs(5), session_list_rx)
        .await
        .expect("timed out waiting for SessionList")
        .expect("host event loop dropped before SessionList");
    assert!(
        sessions.sessions.is_empty(),
        "mock host should start with no resumable sessions"
    );

    let prompt = "runtime split test";
    commands
        .spawn_agent(SpawnAgentPayload {
            name: Some("split-runtime".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent command should succeed");

    let agent = timeout(Duration::from_secs(5), new_agent_rx)
        .await
        .expect("timed out waiting for NewAgent")
        .expect("host event loop dropped before NewAgent");
    assert_eq!(agent.info.name, "split-runtime");

    let AgentEndpoint {
        mut events,
        commands,
        ..
    } = agent;

    let (probe_tx, mut probe_rx) = mpsc::channel::<AgentProbe>(16);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::Bootstrap(payload) => {
                    for event in payload.events {
                        match event {
                            AgentBootstrapEvent::AgentStart(payload) => {
                                if probe_tx
                                    .send(AgentProbe::Started(payload.name))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            AgentBootstrapEvent::ChatEvent(payload) => {
                                if send_chat_probe(&probe_tx, payload).await.is_err() {
                                    return;
                                }
                            }
                            AgentBootstrapEvent::AgentError(_)
                            | AgentBootstrapEvent::SessionSettings(_)
                            | AgentBootstrapEvent::QueuedMessages(_)
                            | AgentBootstrapEvent::AgentActivityStats(_)
                            | AgentBootstrapEvent::HasPriorHistory { .. } => {}
                        }
                    }
                }
                AgentEvent::Start(payload) => {
                    if probe_tx
                        .send(AgentProbe::Started(payload.name))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                AgentEvent::Chat(payload) => {
                    if send_chat_probe(&probe_tx, *payload).await.is_err() {
                        break;
                    }
                }
                AgentEvent::Renamed(_)
                | AgentEvent::SessionSettings(_)
                | AgentEvent::QueuedMessages(_)
                | AgentEvent::SessionHistory(_)
                | AgentEvent::ActivityStats(_) => {}
                AgentEvent::Error(err) => panic!("unexpected agent error: {}", err.message),
            }
        }
    });

    match next_agent_probe(&mut probe_rx, "AgentStart").await {
        AgentProbe::Started(name) => assert_eq!(name, "split-runtime"),
        other => panic!("expected AgentStart probe, got {other:?}"),
    }
    match next_agent_probe(&mut probe_rx, "initial stream end").await {
        AgentProbe::Final(content) => {
            assert!(
                content.ends_with(&format!("mock backend response to: {prompt}")),
                "expected mock response suffix, got: {content}"
            );
        }
        other => panic!("expected initial final response, got {other:?}"),
    }

    let follow_up = "follow-up after background event loop";
    commands
        .send_message(SendMessagePayload {
            message: follow_up.to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        })
        .await
        .expect("follow-up send should succeed");

    match next_agent_probe(&mut probe_rx, "follow-up stream end").await {
        AgentProbe::Final(content) => {
            assert!(
                content.ends_with(&format!("mock backend response to: {follow_up}")),
                "expected mock response suffix, got: {content}"
            );
        }
        other => panic!("expected follow-up final response, got {other:?}"),
    }
}

#[tokio::test]
async fn runtime_preserves_project_bootstrap_until_project_endpoint_is_opened() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let project_root = tempfile::tempdir().expect("create project root");
    let project_path = session_store_dir.path().join("projects.json");
    let project = server::store::project::ProjectStore::load(project_path.clone())
        .expect("load project store")
        .create(
            "runtime-bootstrap-project".to_owned(),
            vec![ProjectRootPath(
                project_root.path().to_string_lossy().to_string(),
            )],
        )
        .expect("create project");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        project_path,
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let HostEndpoint {
        mut events,
        commands,
    } = connect_runtime(host).await;

    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(payload) => {
            assert!(
                payload.projects.iter().any(|item| item.id == project.id),
                "HostBootstrap should include the persisted project"
            );
        }
        _ => panic!("expected initial HostBootstrap"),
    }

    let mut project_endpoint = commands
        .open_project(project.id.clone())
        .await
        .expect("bootstrapped project endpoint should be available");
    match timeout(Duration::from_secs(5), project_endpoint.events.recv())
        .await
        .expect("timed out waiting for project bootstrap")
        .expect("project event stream closed before bootstrap")
    {
        ProjectEvent::Bootstrap(payload) => {
            assert_eq!(payload.project.id, project.id);
            assert_eq!(payload.project.name, project.name);
            assert_eq!(payload.review_summaries.len(), 1);
            assert_eq!(
                payload.review_summaries[0].scope,
                ReviewSummaryScope::Workspace
            );
        }
        _ => panic!("expected ProjectBootstrap"),
    }
}

async fn send_chat_probe(
    tx: &mpsc::Sender<AgentProbe>,
    event: ChatEvent,
) -> Result<(), mpsc::error::SendError<AgentProbe>> {
    match event {
        ChatEvent::StreamEnd(data) => tx.send(AgentProbe::Final(data.message.content)).await,
        _ => Ok(()),
    }
}

async fn connect_runtime(host: server::HostHandle) -> HostEndpoint {
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

    client::connect_host_endpoint(&client_config, client_stream)
        .await
        .expect("runtime handshake failed")
}

async fn connect_raw(host: server::HostHandle) -> client::Connection {
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

async fn next_host_event(events: &mut client::HostEvents, context: &str) -> HostEvent {
    timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for host event: {context}"))
        .unwrap_or_else(|| panic!("host event stream closed while waiting for {context}"))
}

async fn next_agent_probe(rx: &mut mpsc::Receiver<AgentProbe>, context: &str) -> AgentProbe {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for agent probe: {context}"))
        .unwrap_or_else(|| panic!("agent probe channel closed while waiting for {context}"))
}

fn workspace_root() -> String {
    PathBuf::from(".")
        .canonicalize()
        .expect("canonicalize workspace root")
        .to_string_lossy()
        .into_owned()
}
