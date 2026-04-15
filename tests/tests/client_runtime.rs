use std::path::PathBuf;
use std::time::Duration;

use client::{AgentEndpoint, AgentEvent, HostEndpoint, HostEvent};
use protocol::{BackendKind, ChatEvent, SendMessagePayload, SpawnAgentParams, SpawnAgentPayload};
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

    match next_host_event(&mut events, "initial host settings").await {
        HostEvent::HostSettings(_) => {}
        _ => panic!("expected initial HostSettings"),
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
                | HostEvent::ProjectNotify(_)
                | HostEvent::NewTerminal(_) => {}
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
            name: "split-runtime".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
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
                AgentEvent::Start(payload) => {
                    if probe_tx
                        .send(AgentProbe::Started(payload.name))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                AgentEvent::Chat(payload) => match *payload {
                    ChatEvent::MessageAdded(_) => {}
                    ChatEvent::StreamEnd(data) => {
                        if probe_tx
                            .send(AgentProbe::Final(data.message.content))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                },
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
            assert_eq!(content, format!("mock backend response to: {prompt}"))
        }
        other => panic!("expected initial final response, got {other:?}"),
    }

    let follow_up = "follow-up after background event loop";
    commands
        .send_message(SendMessagePayload {
            message: follow_up.to_owned(),
            images: None,
        })
        .await
        .expect("follow-up send should succeed");

    match next_agent_probe(&mut probe_rx, "follow-up stream end").await {
        AgentProbe::Final(content) => {
            assert_eq!(content, format!("mock backend response to: {follow_up}"))
        }
        other => panic!("expected follow-up final response, got {other:?}"),
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
