mod fixture;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use fixture::Fixture;
use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentOrigin, BackendAccessMode, BackendKind,
    ChatEvent, CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, NewAgentPayload,
    SessionId, SpawnAgentParams, SpawnAgentPayload, StreamPath,
};
use server::backend::BackendSession;
use server::backend::mock::MockTurn;
use server::store::session::{SessionRecord, SessionStore};

async fn expect_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = fixture::next_logical_frame_on(client, context).await;
        if is_noise(&env) {
            continue;
        }
        return env;
    }
}

fn is_noise(env: &Envelope) -> bool {
    fixture::is_routine_control_plane_frame(env)
        || matches!(
            env.kind,
            FrameKind::SessionList
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::TaskTokenUsage
                | FrameKind::HostSettings
        )
}

async fn expect_new_agent(client: &mut client::Connection, context: &str) -> NewAgentPayload {
    loop {
        let env = expect_event(client, context).await;
        if env.kind == FrameKind::NewAgent {
            return env.parse_payload().expect("parse NewAgentPayload");
        }
    }
}

async fn expect_new_agent_with_diagnostics(
    client: &mut client::Connection,
    host: &server::HostHandle,
    context: &str,
) -> NewAgentPayload {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = Vec::new();
    let mut deferred = HashMap::<StreamPath, VecDeque<Envelope>>::new();
    loop {
        let env = if let Some(env) = fixture::pop_pending_frame_on(client) {
            env
        } else {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                let registered_agent_ids = host.agent_ids().await;
                panic!(
                    "timed out waiting for {context}; observed post-spawn frames: {observed:#?}; deferred frames: {deferred:#?}; registered agent ids at timeout: {registered_agent_ids:#?}"
                );
            };
            match tokio::time::timeout(remaining, client.next_event()).await {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!(
                    "connection closed before {context}; observed post-spawn frames: {observed:#?}; deferred frames: {deferred:#?}"
                ),
                Ok(Err(error)) => panic!(
                    "next_event failed before {context}: {error:?}; observed post-spawn frames: {observed:#?}; deferred frames: {deferred:#?}"
                ),
                Err(_) => {
                    let registered_agent_ids = host.agent_ids().await;
                    panic!(
                        "timed out waiting for {context}; observed post-spawn frames: {observed:#?}; deferred frames: {deferred:#?}; registered agent ids at timeout: {registered_agent_ids:#?}"
                    );
                }
            }
        };
        let command_error = (env.kind == FrameKind::CommandError).then(|| {
            env.parse_payload::<CommandErrorPayload>()
                .map(|error| format!("{error:?}"))
                .unwrap_or_else(|error| format!("unparseable CommandError: {error}"))
        });
        observed.push(format!(
            "kind={:?} stream={} seq={} command_error={:?} payload={}",
            env.kind, env.stream, env.seq, command_error, env.payload
        ));
        eprintln!(
            "diagnostic stale-parent post-spawn frame: {}",
            observed.last().expect("just pushed diagnostic frame")
        );
        if env.kind == FrameKind::NewAgent {
            for (_, events) in deferred {
                fixture::push_pending_frames_on(client, events);
            }
            return env.parse_payload().expect("parse NewAgentPayload");
        }
        if env.kind == FrameKind::AgentBootstrap {
            let stream = env.stream.clone();
            let events = fixture::agent_bootstrap_frames(&env);
            let bootstrap_event_count = events.len();
            for event in events {
                observed.push(format!(
                    "bootstrap kind={:?} stream={} seq={} payload={}",
                    event.kind, event.stream, event.seq, event.payload
                ));
                deferred.entry(stream.clone()).or_default().push_back(event);
            }
            eprintln!(
                "diagnostic stale-parent AgentBootstrap unpacked: stream={} events={bootstrap_event_count}",
                stream
            );
        } else {
            deferred
                .entry(env.stream.clone())
                .or_default()
                .push_back(env);
        }
    }
}

async fn expect_agent_start(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> protocol::AgentStartPayload {
    let mut deferred = VecDeque::new();
    loop {
        let env = expect_event(client, context).await;
        if env.stream == *stream && env.kind == FrameKind::AgentStart {
            if !deferred.is_empty() {
                fixture::push_pending_frames_on(client, deferred);
            }
            return env.parse_payload().expect("parse AgentStartPayload");
        }
        if env.stream == *stream {
            deferred.push_back(env);
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
    fixture::next_frame_matching_on(client, context, |env| env.kind == FrameKind::CommandError)
        .await
        .parse_payload()
        .expect("parse CommandErrorPayload")
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
            ChatEvent::MessageAdded(message) => {
                text.push_str(&message.content);
                return text;
            }
            ChatEvent::TypingStatusChanged(true) => saw_turn = true,
            ChatEvent::StreamDelta(delta) => text.push_str(&delta.text),
            ChatEvent::StreamEnd(end) => text.push_str(&end.message.content),
            ChatEvent::TypingStatusChanged(false) if saw_turn || !text.is_empty() => return text,
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
    let (parent, parent_start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("Parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp".to_owned()],
                prompt: "parent prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await;

    assert_eq!(parent.new_agent.origin, AgentOrigin::User);
    assert_eq!(parent_start.origin, AgentOrigin::User);
    let parent_start_session_id = parent_start
        .session_id
        .clone()
        .expect("parent AgentStart should include live session_id");
    let parent_initial =
        collect_turn_delta_text(&mut fixture.client, &parent.stream, "parent turn").await;
    assert!(parent_initial.contains("mock backend response to: parent prompt"));

    let sessions = wait_for_session_count(fixture.store_dir(), 1).await;
    let parent_session_id = sessions[0].id.clone();
    assert_eq!(parent_session_id, parent_start_session_id);
    let (_second_client, second_bootstrap) = fixture.connect_with_bootstrap().await;
    let bootstrapped_parent = second_bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == parent.new_agent.agent_id)
        .expect("parent NewAgent in second host bootstrap");
    assert_eq!(
        bootstrapped_parent.session_id.as_ref(),
        Some(&parent_session_id),
        "HostBootstrap NewAgent should retain the live session_id"
    );

    let (child, child_start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("BTW".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.new_agent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: parent_session_id.clone(),
                prompt: "child prompt".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await;

    assert_eq!(child.new_agent.origin, AgentOrigin::SideQuestion);
    assert_eq!(
        child.new_agent.parent_agent_id,
        Some(parent.new_agent.agent_id.clone())
    );
    assert_eq!(child_start.origin, AgentOrigin::SideQuestion);
    assert_eq!(
        child_start.parent_agent_id,
        Some(parent.new_agent.agent_id.clone())
    );
    let child_start_session_id = child_start
        .session_id
        .clone()
        .expect("child AgentStart should include forked session_id");
    assert_ne!(child_start_session_id, parent_session_id);
    let mut child_initial =
        collect_turn_delta_text(&mut fixture.client, &child.stream, "child turn").await;
    if !child_initial.contains("mock backend response to: child prompt") {
        child_initial = collect_turn_delta_text(
            &mut fixture.client,
            &child.stream,
            "child live turn after fork history",
        )
        .await;
    }
    assert!(
        !child_initial.contains("[access_mode: ReadOnly]"),
        "child fork unexpectedly used read-only access mode: {child_initial}"
    );
    assert!(
        child_initial.contains("mock backend response to: child prompt"),
        "unexpected child turn: {child_initial:?}"
    );

    let sessions = wait_for_session_count(fixture.store_dir(), 2).await;
    let child_session = sessions
        .iter()
        .find(|record| record.parent_id.as_ref() == Some(&parent_session_id))
        .expect("child session with parent_id lineage");
    assert_ne!(child_session.id, parent_session_id);
    assert_eq!(child_session.id, child_start_session_id);
    assert_eq!(child_session.backend_kind, BackendKind::Claude);

    fixture
        .mock(&child)
        .await
        .enqueue(MockTurn::history_join())
        .await;
    fixture
        .client
        .send_message(&child.stream, "child follow-up".to_owned())
        .await
        .expect("send child follow-up");
    let child_history =
        collect_turn_delta_text(&mut fixture.client, &child.stream, "child history turn").await;
    assert!(child_history.contains("parent prompt"));
    assert!(child_history.contains("child prompt"));
    assert!(child_history.contains("child follow-up"));

    fixture
        .mock(&parent)
        .await
        .enqueue(MockTurn::history_join())
        .await;
    fixture
        .client
        .send_message(&parent.stream, "parent follow-up".to_owned())
        .await
        .expect("send parent follow-up");
    let parent_history =
        collect_turn_delta_text(&mut fixture.client, &parent.stream, "parent history turn").await;
    assert!(parent_history.contains("parent prompt"));
    assert!(parent_history.contains("parent follow-up"));
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

    let (child, _child_start) = fixture
        .spawn_with(SpawnAgentPayload {
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
        .await;

    assert_eq!(child.new_agent.origin, AgentOrigin::SideQuestion);
    let error = expect_agent_error(&mut fixture.client, &child.stream, "stale fork error").await;
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
            None,
        )
        .expect("insert source session");

    let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
        .expect("spawn host");
    let (mut client, _bootstrap) = fixture::connect_host(host).await;

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
            None,
        )
        .expect("insert parent session");
    let before = load_sessions(dir.path());

    let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
        .expect("spawn real-backend host");
    let (mut client, _bootstrap) = fixture::connect_host(host.clone()).await;

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

    let child =
        expect_new_agent_with_diagnostics(&mut client, &host, "stale-parent child NewAgent").await;
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
