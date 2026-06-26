mod fixture;

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorCode, AgentErrorPayload, AgentId,
    AgentOrigin, BackendAccessMode, BackendKind, ChatEvent, CommandErrorCode, CommandErrorPayload,
    Envelope, FrameError, FrameKind, HostBootstrapPayload, NewAgentPayload, SessionId,
    SpawnAgentParams, SpawnAgentPayload, StreamPath,
};
use server::backend::BackendSession;
use server::store::session::{SessionRecord, SessionStore};

async fn connect_host(host: server::HostHandle) -> (client::Connection, HostBootstrapPayload) {
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

    let mut client = client::connect(&client_config, client_stream)
        .await
        .expect("client handshake failed");
    let env = next_raw_event(&mut client, "host bootstrap")
        .await
        .expect("host bootstrap read failed")
        .expect("connection closed before host bootstrap");
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap = env.parse_payload().expect("parse HostBootstrapPayload");
    (client, bootstrap)
}

async fn next_raw_event(
    client: &mut client::Connection,
    context: &str,
) -> Result<Option<Envelope>, FrameError> {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(result) => result,
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn expect_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let pending_key = client as *mut client::Connection as usize;
        if let Some(env) = pop_pending_agent_event(pending_key) {
            if fixture::is_builtin_team_custom_agent_notify(&env) || is_noise(&env) {
                continue;
            }
            return env;
        }
        let env = next_raw_event(client, context)
            .await
            .unwrap_or_else(|err| panic!("next_event failed before {context}: {err:?}"))
            .unwrap_or_else(|| panic!("connection closed before {context}"));
        if fixture::is_builtin_team_custom_agent_notify(&env) || is_noise(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("AgentBootstrap");
            if let Some(first) = record_agent_bootstrap_events(pending_key, &env.stream, bootstrap)
            {
                return first;
            }
            continue;
        }
        return env;
    }
}

type PendingAgentEvents = HashMap<usize, HashMap<StreamPath, VecDeque<Envelope>>>;

fn pending_agent_events() -> &'static Mutex<PendingAgentEvents> {
    static PENDING: OnceLock<Mutex<PendingAgentEvents>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_agent_bootstrap_events(
    pending_key: usize,
    stream: &StreamPath,
    bootstrap: AgentBootstrapPayload,
) -> Option<Envelope> {
    let mut events = bootstrap
        .events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| agent_bootstrap_event_envelope(stream, index as u64, event));
    let first = events.next();
    let mut rest = events.collect::<VecDeque<_>>();
    if !rest.is_empty() {
        pending_agent_events()
            .lock()
            .expect("pending agent event mutex poisoned")
            .entry(pending_key)
            .or_default()
            .entry(stream.clone())
            .or_default()
            .append(&mut rest);
    }
    first
}

fn pop_pending_agent_event(pending_key: usize) -> Option<Envelope> {
    let mut pending = pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned");
    let streams = pending.get_mut(&pending_key)?;
    let stream = streams.keys().next().cloned()?;
    let queue = streams
        .get_mut(&stream)
        .expect("pending stream key disappeared while popping");
    let env = queue.pop_front();
    if queue.is_empty() {
        streams.remove(&stream);
    }
    if streams.is_empty() {
        pending.remove(&pending_key);
    }
    env
}

fn is_noise(env: &Envelope) -> bool {
    matches!(
        env.kind,
        FrameKind::SessionSettings
            | FrameKind::QueuedMessages
            | FrameKind::SessionList
            | FrameKind::SessionSchemas
            | FrameKind::BackendSetup
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::HostSettings
    )
}

fn agent_bootstrap_event_envelope(
    stream: &StreamPath,
    seq: u64,
    event: AgentBootstrapEvent,
) -> Option<Envelope> {
    match event {
        AgentBootstrapEvent::AgentStart(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentStart,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::AgentError(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentError,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::SessionSettings(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::SessionSettings,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::QueuedMessages(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::QueuedMessages,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::ChatEvent(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::HasPriorHistory { .. } => None,
    }
    .map(|result| result.expect("serialize synthetic bootstrap event"))
}

async fn expect_new_agent(client: &mut client::Connection, context: &str) -> NewAgentPayload {
    loop {
        let env = expect_event(client, context).await;
        if env.kind == FrameKind::NewAgent {
            return env.parse_payload().expect("parse NewAgentPayload");
        }
    }
}

async fn expect_agent_start(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> protocol::AgentStartPayload {
    loop {
        let env = expect_event(client, context).await;
        if env.stream == *stream && env.kind == FrameKind::AgentStart {
            return env.parse_payload().expect("parse AgentStartPayload");
        }
    }
}

async fn expect_agent_error(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentErrorPayload {
    loop {
        let env = expect_event(client, context).await;
        if env.stream == *stream && env.kind == FrameKind::AgentError {
            return env.parse_payload().expect("parse AgentErrorPayload");
        }
    }
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    loop {
        let env = expect_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            return env.parse_payload().expect("parse CommandErrorPayload");
        }
    }
}

async fn collect_turn_delta_text(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> String {
    let mut text = String::new();
    let mut saw_turn = false;
    loop {
        let env = expect_event(client, context).await;
        if env.stream != *stream || env.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::TypingStatusChanged(true) => saw_turn = true,
            ChatEvent::StreamDelta(delta) => text.push_str(&delta.text),
            ChatEvent::StreamEnd(end) => text.push_str(&end.message.content),
            ChatEvent::TypingStatusChanged(false) if saw_turn => return text,
            _ => {}
        }
    }
}

fn load_sessions(store_dir: &std::path::Path) -> Vec<SessionRecord> {
    let store = SessionStore::load(store_dir.join("sessions.json")).expect("load session store");
    store.list().expect("list sessions")
}

async fn wait_for_session_count(store_dir: &std::path::Path, count: usize) -> Vec<SessionRecord> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = load_sessions(store_dir);
        if sessions.len() == count {
            return sessions;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} sessions, saw {}",
            sessions.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn mock_fork_creates_interactive_side_question_with_lineage() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp".to_owned()],
                prompt: "parent prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent");

    let parent = expect_new_agent(&mut fixture.client, "parent NewAgent").await;
    assert_eq!(parent.origin, AgentOrigin::User);
    let parent_start = expect_agent_start(
        &mut fixture.client,
        &parent.instance_stream,
        "parent AgentStart",
    )
    .await;
    assert_eq!(parent_start.origin, AgentOrigin::User);
    let parent_start_session_id = parent_start
        .session_id
        .clone()
        .expect("parent AgentStart should include live session_id");
    let parent_initial =
        collect_turn_delta_text(&mut fixture.client, &parent.instance_stream, "parent turn").await;
    assert!(parent_initial.contains("mock backend response to: parent prompt"));

    let sessions = wait_for_session_count(fixture.store_dir(), 1).await;
    let parent_session_id = sessions[0].id.clone();
    assert_eq!(parent_session_id, parent_start_session_id);
    let (_second_client, second_bootstrap) = fixture.connect_with_bootstrap().await;
    let bootstrapped_parent = second_bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == parent.agent_id)
        .expect("parent NewAgent in second host bootstrap");
    assert_eq!(
        bootstrapped_parent.session_id.as_ref(),
        Some(&parent_session_id),
        "HostBootstrap NewAgent should retain the live session_id"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("BTW".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: parent_session_id.clone(),
                prompt: "child prompt".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("spawn side question");

    let child = expect_new_agent(&mut fixture.client, "child NewAgent").await;
    assert_eq!(child.origin, AgentOrigin::SideQuestion);
    assert_eq!(child.parent_agent_id, Some(parent.agent_id.clone()));
    let child_start = expect_agent_start(
        &mut fixture.client,
        &child.instance_stream,
        "child AgentStart",
    )
    .await;
    assert_eq!(child_start.origin, AgentOrigin::SideQuestion);
    assert_eq!(child_start.parent_agent_id, Some(parent.agent_id.clone()));
    let child_start_session_id = child_start
        .session_id
        .clone()
        .expect("child AgentStart should include forked session_id");
    assert_ne!(child_start_session_id, parent_session_id);
    let child_initial =
        collect_turn_delta_text(&mut fixture.client, &child.instance_stream, "child turn").await;
    assert!(
        child_initial.contains("[access_mode: ReadOnly]"),
        "child initial response did not show read-only access mode: {child_initial}"
    );
    assert!(child_initial.contains("mock backend response to: child prompt"));

    let sessions = wait_for_session_count(fixture.store_dir(), 2).await;
    let child_session = sessions
        .iter()
        .find(|record| record.parent_id.as_ref() == Some(&parent_session_id))
        .expect("child session with parent_id lineage");
    assert_ne!(child_session.id, parent_session_id);
    assert_eq!(child_session.id, child_start_session_id);
    assert_eq!(child_session.backend_kind, BackendKind::Claude);

    fixture
        .client
        .send_message(
            &child.instance_stream,
            "__mock_history__ child follow-up".to_owned(),
        )
        .await
        .expect("send child follow-up");
    let child_history = collect_turn_delta_text(
        &mut fixture.client,
        &child.instance_stream,
        "child history turn",
    )
    .await;
    assert!(child_history.contains("parent prompt"));
    assert!(child_history.contains("child prompt"));
    assert!(child_history.contains("__mock_history__ child follow-up"));

    fixture
        .client
        .send_message(
            &parent.instance_stream,
            "__mock_history__ parent follow-up".to_owned(),
        )
        .await
        .expect("send parent follow-up");
    let parent_history = collect_turn_delta_text(
        &mut fixture.client,
        &parent.instance_stream,
        "parent history turn",
    )
    .await;
    assert!(parent_history.contains("parent prompt"));
    assert!(parent_history.contains("__mock_history__ parent follow-up"));
    assert!(
        !parent_history.contains("child prompt"),
        "parent history was mutated by child fork: {parent_history}"
    );
}

#[tokio::test]
async fn server_rejects_fork_without_parent_or_source_session() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("invalid fork".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: SessionId("parent-session".to_owned()),
                prompt: "side question".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("send fork without parent");
    let error = expect_command_error(&mut fixture.client, "fork without parent error").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(error.message.contains("parent_agent_id"));

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("invalid fork".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(AgentId("parent-agent".to_owned())),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: SessionId(String::new()),
                prompt: "side question".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("send fork without source session");
    let error = expect_command_error(&mut fixture.client, "fork without source error").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(error.message.contains("from_session_id"));
}

#[tokio::test]
async fn stale_fork_source_session_fails_as_agent_error() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Stale BTW".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(AgentId("missing-parent-agent".to_owned())),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: SessionId("stale-source-session".to_owned()),
                prompt: "side question".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("send stale fork spawn");

    let child = expect_new_agent(&mut fixture.client, "stale fork NewAgent").await;
    assert_eq!(child.origin, AgentOrigin::SideQuestion);
    let _ = expect_agent_start(
        &mut fixture.client,
        &child.instance_stream,
        "stale fork start",
    )
    .await;
    let error = expect_agent_error(
        &mut fixture.client,
        &child.instance_stream,
        "stale fork error",
    )
    .await;
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(error.message.contains("cannot fork missing session"));
}

#[tokio::test]
async fn fork_rejects_orphan_parent_even_when_source_session_exists() {
    fixture::init_tracing();
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    let project_path = dir.path().join("projects.json");
    let settings_path = dir.path().join("settings.json");
    let parent_session_id = SessionId("source-session".to_owned());
    let store = SessionStore::load(session_path.clone()).expect("load session store");
    store
        .upsert_backend_session(
            &BackendSession {
                id: parent_session_id.clone(),
                backend_kind: BackendKind::Claude,
                workspace_roots: vec!["/tmp".to_owned()],
                title: Some("Source".to_owned()),
                token_count: None,
                created_at_ms: Some(100),
                updated_at_ms: Some(100),
                resumable: true,
            },
            None,
            None,
            None,
        )
        .expect("insert source session");

    let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
        .expect("spawn host");
    let (mut client, _bootstrap) = connect_host(host).await;

    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Orphan BTW".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(AgentId("orphan-parent-agent".to_owned())),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: parent_session_id,
                prompt: "side question".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("send orphan-parent fork spawn");

    let child = expect_new_agent(&mut client, "orphan fork NewAgent").await;
    assert_eq!(child.origin, AgentOrigin::SideQuestion);
    let _ = expect_agent_start(&mut client, &child.instance_stream, "orphan fork start").await;
    let error = expect_agent_error(&mut client, &child.instance_stream, "orphan fork error").await;
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(error.message.contains("parent_agent_id"));
    assert!(error.message.contains("is not running"));
}

#[tokio::test]
async fn stale_parent_fork_fails_without_touching_source_session() {
    fixture::init_tracing();
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    let project_path = dir.path().join("projects.json");
    let settings_path = dir.path().join("settings.json");
    let parent_session_id = SessionId("codex-parent-session".to_owned());
    let store = SessionStore::load(session_path.clone()).expect("load session store");
    store
        .upsert_backend_session(
            &BackendSession {
                id: parent_session_id.clone(),
                backend_kind: BackendKind::Codex,
                workspace_roots: vec!["/tmp".to_owned()],
                title: Some("Codex parent".to_owned()),
                token_count: None,
                created_at_ms: Some(100),
                updated_at_ms: Some(100),
                resumable: true,
            },
            None,
            None,
            None,
        )
        .expect("insert parent session");
    let before = load_sessions(dir.path());

    let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
        .expect("spawn real-backend host");
    let (mut client, _bootstrap) = connect_host(host).await;

    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Unsupported BTW".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(AgentId("codex-parent-agent".to_owned())),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: parent_session_id.clone(),
                prompt: "side question".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("send stale-parent fork spawn");

    let child = expect_new_agent(&mut client, "stale-parent child NewAgent").await;
    assert_eq!(child.origin, AgentOrigin::SideQuestion);
    assert_eq!(child.backend_kind, BackendKind::Codex);
    assert_eq!(
        child.parent_agent_id,
        Some(AgentId("codex-parent-agent".to_owned()))
    );
    let _ = expect_agent_start(&mut client, &child.instance_stream, "failed child start").await;
    let error = expect_agent_error(
        &mut client,
        &child.instance_stream,
        "fork stale-parent error",
    )
    .await;
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(error.message.contains("parent_agent_id"));
    assert!(error.message.contains("is not running"));

    let after = load_sessions(dir.path());
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].updated_at_ms, before[0].updated_at_ms);
    assert_eq!(after[0].parent_id, before[0].parent_id);
}
