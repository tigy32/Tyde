mod fixture;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorPayload, AgentStartPayload, BackendKind,
    ChatEvent, DeleteSessionPayload, Envelope, FetchSessionHistoryPayload, FrameKind,
    ListSessionsPayload, NewAgentPayload, Project, ProjectCreatePayload, ProjectNotifyPayload,
    ProjectRootPath, SessionHistoryPayload, SessionId, SessionListPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};
use server::backend::mock::{MockScript, MockTurn};
use server::store::session::SessionStore;
use std::path::Path;
use std::time::Duration;

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = fixture::next_logical_frame_on(client, context).await;
        eprintln!("TYDE SESSION RESUME WAIT context={context} frame={env:?}");
        if fixture::is_routine_control_plane_frame(&env)
            || matches!(
                env.kind,
                FrameKind::HostSettings
                    | FrameKind::BackendCapacity
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::SessionList
                    | FrameKind::SessionSummaryCountUpdated
                    | FrameKind::TaskTokenUsage
                    | FrameKind::WorkflowNotify
                    | FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::AgentActivityStats
                    | FrameKind::ContextCompactionNotify
                    | FrameKind::ContextCompactionCapability
            )
        {
            continue;
        }
        return env;
    }
}

async fn expect_raw_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    fixture::next_frame_matching_on(client, context, |env| {
        if fixture::is_routine_control_plane_frame(env) {
            return false;
        }
        if env.stream != *stream {
            return false;
        }
        if env.kind == kind {
            return true;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::ContextCompactionNotify
                | FrameKind::ContextCompactionCapability
        ) {
            return false;
        }
        panic!(
            "wait for {kind} on {stream} during {context} received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    })
    .await
}

async fn expect_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> ChatEvent {
    loop {
        let env = expect_next_event(client, context).await;
        if env.stream != *stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ChatEvent);
        return env.parse_payload().expect("failed to parse ChatEvent");
    }
}

async fn expect_chat_event(client: &mut client::Connection, context: &str) -> ChatEvent {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::ChatEvent {
            continue;
        }
        return env.parse_payload().expect("failed to parse ChatEvent");
    }
}

async fn expect_agent_start_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.stream != *stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::AgentStart);
        return env.parse_payload().expect("failed to parse AgentStart");
    }
}

async fn expect_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let event =
        expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true) or StreamStart")
            .await;
    let delta = match event {
        ChatEvent::TypingStatusChanged(true) => {
            let event = expect_chat_event_on_stream(client, stream, "StreamStart").await;
            match event {
                ChatEvent::StreamStart(_) => {
                    expect_chat_event_on_stream(client, stream, "StreamDelta").await
                }
                delta @ ChatEvent::StreamDelta(_) => delta,
                other => panic!("expected StreamStart or StreamDelta, got {other:?}"),
            }
        }
        ChatEvent::StreamStart(_) => {
            expect_chat_event_on_stream(client, stream, "StreamDelta").await
        }
        delta @ ChatEvent::StreamDelta(_) => delta,
        other => panic!("expected TypingStatusChanged(true) or StreamStart, got {other:?}"),
    };
    match &delta {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let event = expect_chat_event_on_stream(client, stream, "StreamEnd").await;
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let event = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

fn assert_bootstrap_prior_history_indicator(
    payload: &AgentBootstrapPayload,
    expected_message_count: u32,
) -> u64 {
    let before_seq = payload.events.iter().find_map(|event| match event {
        AgentBootstrapEvent::HasPriorHistory {
            message_count,
            before_seq,
        } if *message_count == expected_message_count => Some(*before_seq),
        _ => None,
    });
    assert!(
        before_seq.is_some(),
        "AgentBootstrap should include HasPriorHistory({expected_message_count}), got {:?}",
        payload.events
    );
    before_seq.expect("checked above")
}

fn assert_bootstrap_has_no_prior_history_indicator(payload: &AgentBootstrapPayload) {
    assert!(
        payload
            .events
            .iter()
            .all(|event| !matches!(event, AgentBootstrapEvent::HasPriorHistory { .. })),
        "AgentBootstrap should not include HasPriorHistory: {:?}",
        payload.events
    );
}

fn assert_bootstrap_tail_messages(
    payload: &AgentBootstrapPayload,
    expected_chronological: &[&str],
) {
    let contents = payload
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) => {
                Some(message.content.as_str())
            }
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => {
                Some(end.message.content.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        contents.len(),
        expected_chronological.len(),
        "unexpected bootstrap tail messages: {:?}",
        payload.events
    );
    for (content, expected) in contents.iter().zip(expected_chronological) {
        assert!(
            content.contains(expected),
            "bootstrap message content {content:?} did not contain {expected:?}",
        );
    }
}

/// Every message body a bootstrap carries, in the order the user would read
/// them.
fn bootstrap_message_contents(payload: &AgentBootstrapPayload) -> Vec<String> {
    payload
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) => {
                Some(message.content.clone())
            }
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => {
                Some(end.message.content.clone())
            }
            _ => None,
        })
        .collect()
}

fn bootstrap_agent_start(payload: &AgentBootstrapPayload) -> &AgentStartPayload {
    payload
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentStart(start) => Some(start),
            _ => None,
        })
        .expect("AgentBootstrap should include AgentStart")
}

async fn fetch_history_page(
    client: &mut client::Connection,
    stream: &StreamPath,
    agent_id: protocol::AgentId,
    before_seq: Option<u64>,
    limit: u32,
) -> SessionHistoryPayload {
    client
        .fetch_session_history(
            stream,
            FetchSessionHistoryPayload {
                agent_id,
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
                before_seq,
                limit,
            },
        )
        .await
        .expect("fetch_session_history failed");

    let env =
        expect_raw_event_on_stream(client, stream, FrameKind::SessionHistory, "SessionHistory")
            .await;
    env.parse_payload()
        .expect("failed to parse SessionHistoryPayload")
}

// Pages carry events in the order they happened, exactly like the live
// stream: both clients replay a page front to back before prepending it, so a
// reversed page renders each loaded window upside down and attaches tool
// cards to the wrong turns.
fn assert_history_page(
    page: &SessionHistoryPayload,
    expected_oldest_first: &[&str],
    expected_has_more_before: bool,
) {
    assert_eq!(
        page.events.len(),
        expected_oldest_first.len() * 3,
        "unexpected SessionHistory event count: {:?}",
        page.events
    );
    for (response, expected) in page
        .events
        .as_chunks::<3>()
        .0
        .iter()
        .zip(expected_oldest_first)
    {
        let [
            ChatEvent::StreamStart(_),
            ChatEvent::StreamDelta(delta),
            ChatEvent::StreamEnd(end),
        ] = response
        else {
            panic!("expected chronological response boundary, got {response:?}");
        };
        assert!(
            end.message.content.contains(expected),
            "history message content {:?} did not contain {expected:?}",
            end.message.content
        );
        assert!(
            delta.text.contains(expected),
            "history delta text {:?} did not contain {expected:?}",
            delta.text
        );
    }
    assert_eq!(page.has_more_before, expected_has_more_before);
}

async fn wait_for_session_list(
    client: &mut client::Connection,
    context: &str,
) -> SessionListPayload {
    let env = fixture::next_frame_matching_on(client, context, |env| {
        if fixture::is_routine_control_plane_frame(env) {
            return false;
        }
        if env.kind == FrameKind::AgentBootstrap {
            return false;
        }
        // A live capability update can arrive after deletion, before SessionList.
        // It does not change whether the deleted session remains in that list.
        if env.kind == FrameKind::ContextCompactionCapability {
            return false;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::NewAgent
                | FrameKind::AgentStart
                | FrameKind::AgentError
                | FrameKind::SessionSummaryCountUpdated
                | FrameKind::TaskTokenUsage
                | FrameKind::ChatEvent
        ) {
            return false;
        }
        if env.kind == FrameKind::SessionList {
            return true;
        }
        panic!(
            "wait_for_session_list({context}) received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    })
    .await;
    env.parse_payload()
        .expect("failed to parse SessionListPayload")
}

async fn expect_turn(client: &mut client::Connection, expected_text: &str) {
    let event = expect_chat_event(client, "TypingStatusChanged(true) or StreamStart").await;
    let delta = match event {
        ChatEvent::TypingStatusChanged(true) => {
            let event = expect_chat_event(client, "StreamStart").await;
            match event {
                ChatEvent::StreamStart(_) => expect_chat_event(client, "StreamDelta").await,
                delta @ ChatEvent::StreamDelta(_) => delta,
                other => panic!("expected StreamStart or StreamDelta, got {other:?}"),
            }
        }
        ChatEvent::StreamStart(_) => expect_chat_event(client, "StreamDelta").await,
        delta @ ChatEvent::StreamDelta(_) => delta,
        other => panic!("expected TypingStatusChanged(true) or StreamStart, got {other:?}"),
    };

    match &delta {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let event = expect_chat_event(client, "StreamEnd").await;
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let event = expect_chat_event(client, "TypingStatusChanged(false)").await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    fixture::assert_no_interesting_frame_on(client, duration, context, |env| {
        matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
        )
    })
    .await;
}

async fn expect_no_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    duration: Duration,
    context: &str,
) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env))) if fixture::is_builtin_team_custom_agent_notify(&env) => continue,
            Ok(Ok(Some(env))) if env.stream == *stream && env.kind == FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse unexpected ChatEvent");
                panic!("unexpected live ChatEvent on {stream} before {context}: {event:?}");
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    env.parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
}

#[tokio::test]
async fn list_sessions_and_resume_agent() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumable".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resumable agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _: NewAgentPayload = env.parse_payload().expect("parse NewAgent");

    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one stored session");
    let session = &list.sessions[0];
    assert_eq!(session.backend_kind, BackendKind::Claude);
    assert_eq!(session.workspace_roots, vec!["/tmp/test".to_owned()]);
    assert!(session.resumable);
    assert_eq!(session.message_count, 1);

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");

    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");
    let start = bootstrap_agent_start(&payload);
    assert_eq!(start.agent_id, resumed.agent_id);
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after resume failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList after resume").await;
    assert_eq!(
        list.sessions.len(),
        1,
        "resume should reuse the same session"
    );
    assert_eq!(list.sessions[0].id, session.id);
    assert_eq!(list.sessions[0].message_count, 2);
}

fn rewrite_sessions_json_with_foreign_record(path: &Path, foreign_id: &str) {
    let contents = std::fs::read_to_string(path).expect("read sessions.json");
    let value: serde_json::Value = serde_json::from_str(&contents).expect("parse sessions.json");
    let write_seq = value
        .get("write_seq")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let Some(serde_json::Value::Object(mut records)) = value.get("records").cloned() else {
        panic!("sessions.json records must be an object");
    };
    records.insert(
        foreign_id.to_owned(),
        serde_json::json!({
            "id": foreign_id,
            "backend_kind": "claude",
            "workspace_roots": ["/tmp/foreign"],
            "created_at_ms": 1,
            "updated_at_ms": 1,
        }),
    );
    #[derive(serde::Serialize)]
    struct SessionsFile<'a> {
        records: &'a serde_json::Map<String, serde_json::Value>,
        write_seq: u64,
    }
    let body = serde_json::to_string(&SessionsFile {
        write_seq: write_seq + 1,
        records: &records,
    })
    .expect("serialize foreign sessions.json");
    std::fs::write(path, body).expect("write sessions.json");
}

#[tokio::test]
async fn session_store_keeps_foreign_records_across_turn() {
    let mut fixture = Fixture::new_with_store_files(
        r#"{"records": {}, "write_seq": 41}"#,
        r#"{"version": 2, "records": {}, "write_seq": 17}"#,
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("live-session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn live session failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");

    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, "SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one stored session");
    let session_id = list.sessions[0].id.clone();
    assert_eq!(list.sessions[0].message_count, 1);

    let sessions_path = fixture.store_dir().join("sessions.json");
    rewrite_sessions_json_with_foreign_record(&sessions_path, "foreign-session");

    fixture
        .client
        .send_message(&new_agent.instance_stream, "follow-up".to_owned())
        .await
        .expect("send follow-up failed");
    expect_turn(&mut fixture.client, "mock backend response to: follow-up").await;

    let store = SessionStore::load(sessions_path).expect("load session store");
    let records = store.list().expect("list session records");
    assert_eq!(records.len(), 2, "live write must keep the foreign record");
    let live = records
        .iter()
        .find(|record| record.id == session_id)
        .expect("live session record");
    assert_eq!(live.message_count, 2);
    assert!(
        records
            .iter()
            .any(|record| record.id.0 == "foreign-session"),
        "foreign session record missing after live turn: {:?}",
        records
            .iter()
            .map(|record| record.id.0.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn opening_agent_bootstrap_loads_tail_and_gates_older_history() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("history-on-demand".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/history-on-demand".to_owned()],
                prompt: "history 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn history agent failed");

    let env = expect_next_event(&mut fixture.client, "history NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse history NewAgent");
    let _ = expect_next_event(&mut fixture.client, "history AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: history 0").await;

    for index in 1..55 {
        let prompt = format!("history {index}");
        fixture
            .client
            .send_message(&new_agent.instance_stream, prompt.clone())
            .await
            .expect("send history follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_agent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running history agent");

    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_agent_stream,
        FrameKind::AgentBootstrap,
        "history AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let gate_before_seq = assert_bootstrap_prior_history_indicator(&payload, 40);
    let expected_tail_strings = (40..55)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    let first_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(gate_before_seq),
        2,
    )
    .await;
    assert_history_page(&first_page, &["history 38", "history 39"], true);
    let first_cursor = first_page
        .oldest_seq
        .expect("first history page should include an oldest_seq cursor");

    let second_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(first_cursor),
        2,
    )
    .await;
    assert_history_page(&second_page, &["history 36", "history 37"], true);
    let second_cursor = second_page
        .oldest_seq
        .expect("second history page should include an oldest_seq cursor");

    let third_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(second_cursor),
        50,
    )
    .await;
    let expected_final_strings = (0..=35)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_final = expected_final_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_history_page(&third_page, &expected_final, false);
}

#[tokio::test]
async fn first_history_fetch_uses_bootstrap_gate_cursor_without_live_dupes() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("history-no-dupe".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/history-no-dupe".to_owned()],
                prompt: "prior 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn history no-dupe agent failed");

    let env = expect_next_event(&mut fixture.client, "no-dupe NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse no-dupe NewAgent");
    let _ = expect_next_event(&mut fixture.client, "no-dupe AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: prior 0").await;
    for index in 1..51 {
        let prompt = format!("prior {index}");
        fixture
            .client
            .send_message(&new_agent.instance_stream, prompt.clone())
            .await
            .expect("send history no-dupe follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_agent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running history agent");

    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_agent_stream,
        FrameKind::AgentBootstrap,
        "no-dupe AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let gate_before_seq = assert_bootstrap_prior_history_indicator(&payload, 36);
    let expected_tail_strings = (36..51)
        .map(|index| format!("prior {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    second_client
        .send_message(&second_agent_stream, "new visible message".to_owned())
        .await
        .expect("send visible follow-up failed");
    expect_turn_on_stream(
        &mut second_client,
        &second_agent_stream,
        "mock backend response to: new visible message",
    )
    .await;

    let first_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(gate_before_seq),
        10,
    )
    .await;
    assert_history_page(
        &first_page,
        &[
            "prior 26", "prior 27", "prior 28", "prior 29", "prior 30", "prior 31", "prior 32",
            "prior 33", "prior 34", "prior 35",
        ],
        true,
    );
    assert!(
        first_page.events.iter().all(|event| match event {
            ChatEvent::MessageAdded(message) => !message.content.contains("new visible message"),
            ChatEvent::StreamDelta(delta) => !delta.text.contains("new visible message"),
            ChatEvent::StreamEnd(end) => !end.message.content.contains("new visible message"),
            _ => true,
        }),
        "first history fetch must not duplicate live rows: {:?}",
        first_page.events
    );
}

#[tokio::test]
async fn resume_long_replay_history_stays_capped_without_live_broadcast() {
    // Regression: restoring a long stored session must bootstrap the resuming
    // client with only the 15-message tail (plus the prior-history indicator),
    // never live-broadcast the entire replayed transcript. The single-message
    // sibling test below drains one event before the resume-replay barrier is
    // selected, so it cannot catch the race where the barrier closes the gate
    // while many replay events are still buffered on the backend stream.
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-long-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-long".to_owned()],
                prompt: "history 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume long source failed");

    let env = expect_next_event(&mut fixture.client, "resume long source NewAgent").await;
    let source: NewAgentPayload = env
        .parse_payload()
        .expect("parse resume long source NewAgent");
    let _ = expect_next_event(&mut fixture.client, "resume long source AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: history 0").await;

    // Build a 30-message transcript so many replay events are buffered at once.
    for index in 1..30 {
        let prompt = format!("history {index}");
        fixture
            .client
            .send_message(&source.instance_stream, prompt.clone())
            .await
            .expect("send resume long follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before long resume failed");
    let list = wait_for_session_list(&mut fixture.client, "SessionList before long resume").await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-long".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume long agent failed");

    let env = expect_next_event(&mut fixture.client, "resume long NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resume long NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resume long AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");

    // 30 replayed messages -> 15 gated behind the prior-history indicator, 15 in the tail.
    assert_bootstrap_prior_history_indicator(&payload, 15);
    let expected_tail_strings = (15..30)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    // The gated (older) portion of the transcript must not leak as live events.
    expect_no_chat_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        Duration::from_millis(300),
        "replayed history must not live-broadcast",
    )
    .await;
}

#[tokio::test]
async fn async_resume_replay_history_is_ingested_without_live_broadcast() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-no-leak-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-no-leak".to_owned()],
                prompt: "original history".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume no-leak source failed");

    let _ = expect_next_event(&mut fixture.client, "resume source NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "resume source AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: original history",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before resume failed");
    let list =
        wait_for_session_list(&mut fixture.client, "SessionList before no-leak resume").await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-no-leak".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume no-leak agent failed");

    let env = expect_next_event(&mut fixture.client, "resume no-leak NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resume no-leak NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resume no-leak AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &["original history"]);

    expect_no_chat_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        Duration::from_millis(150),
        "new turn after quiet resume",
    )
    .await;

    fixture
        .client
        .send_message(&resumed.instance_stream, "new turn after resume".to_owned())
        .await
        .expect("send after no-leak resume failed");
    expect_turn_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        "mock backend response to: new turn after resume",
    )
    .await;
}

#[tokio::test]
async fn resume_backend_close_before_barrier_flushes_eager_attach_with_fatal_error() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-close-before-barrier-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-close-before-barrier".to_owned()],
                prompt: "close before barrier seed".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume close-before-barrier source failed");

    let _ = expect_next_event(&mut fixture.client, "close-before-barrier source NewAgent").await;
    let _ = expect_next_event(
        &mut fixture.client,
        "close-before-barrier source AgentStart",
    )
    .await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: close before barrier seed",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before close-before-barrier resume failed");
    let list = wait_for_session_list(
        &mut fixture.client,
        "SessionList before close-before-barrier resume",
    )
    .await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    let close_reservation = fixture
        .reserve_next_mock_resume_closing_before_barrier("resume-close-before-barrier")
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-close-before-barrier".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume close-before-barrier agent failed");

    let env = expect_next_event(&mut fixture.client, "close-before-barrier resumed NewAgent").await;
    drop(close_reservation);
    let resumed: NewAgentPayload = env
        .parse_payload()
        .expect("parse close-before-barrier resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "close-before-barrier AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let error = payload
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("AgentBootstrap should surface fatal resume barrier error");
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("agent backend closed before resume replay completed"),
        "unexpected fatal error: {}",
        error.message
    );
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &[]);
}

#[tokio::test]
async fn agent_bootstrap_keeps_active_stream_while_recent_history_loads() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("active-history-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/active-history-parent".to_owned()],
                prompt: "parent ready".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "active parent NewAgent").await;
    let parent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent.instance_stream,
        "active parent AgentStart",
    )
    .await;
    expect_turn_on_stream(
        &mut fixture.client,
        &parent.instance_stream,
        "mock backend response to: parent ready",
    )
    .await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "active-history-child",
            MockScript::one(MockTurn::held_text("child active held open")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("active-history-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/active-history-child".to_owned()],
                prompt: "child active".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let env = expect_next_event(&mut fixture.client, "active child NewAgent").await;
    let child: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "active child AgentStart",
    )
    .await;
    loop {
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &child.instance_stream,
            "held child StreamEnd",
        )
        .await;
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }
    drop(reservation);

    fixture
        .mock_by_id(&parent.agent_id)
        .await
        .enqueue(MockTurn::agent_control_await(vec![child.agent_id.clone()]))
        .await;
    fixture
        .client
        .send_message(
            &parent.instance_stream,
            format!("await child {}", child.agent_id.0),
        )
        .await
        .expect("send parent await prompt failed");

    loop {
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &parent.instance_stream,
            "parent active ToolRequest",
        )
        .await;
        if matches!(event, ChatEvent::ToolRequest(_)) {
            break;
        }
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_parent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == parent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running parent agent");
    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_parent_stream,
        FrameKind::AgentBootstrap,
        "active parent AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &["parent ready"]);
    let active_start_index = payload
        .events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(_))
            )
        })
        .expect("active StreamStart should be replayed in AgentBootstrap");
    let active_events = &payload.events[active_start_index + 1..];
    assert!(
        active_events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(request))
                if fixture::tool_request_name(request) == "tyde_await_agents"
        )),
        "active tool request should be replayed in AgentBootstrap: {:?}",
        payload.events
    );
    assert!(
        active_events.iter().all(|event| !matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(_))
        )),
        "active response should remain open after its final StreamStart: {:?}",
        payload.events
    );

    fixture
        .client
        .interrupt(&child.instance_stream)
        .await
        .expect("interrupt held child");
}

#[tokio::test]
async fn session_listing_covers_empty_parent_child_and_resume_without_prompt() {
    let mut fixture = Fixture::new().await;
    // A completed bootstrap reports turn_active=false instead of replaying a
    // live idle event. Hold these turns until subscription so expect_turn
    // always checks live streaming, even when the mock finishes immediately.
    let parent_gate = server::backend::mock::MockGateHandle::new();
    let child_gate = server::backend::mock::MockGateHandle::new();
    let parent_reservation = fixture
        .reserve_next_mock_launch(
            "parent",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: parent hello",
                &parent_gate,
            )),
        )
        .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("initial list_sessions failed");

    let list = wait_for_session_list(&mut fixture.client, "initial empty SessionList").await;
    assert!(
        list.sessions.is_empty(),
        "expected no sessions before any spawn"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/parent".to_owned()],
                prompt: "parent hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    let parent_new_agent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let _ = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    parent_gate.release_one();
    drop(parent_reservation);
    expect_turn(
        &mut fixture.client,
        "mock backend response to: parent hello",
    )
    .await;

    let child_reservation = fixture
        .reserve_next_mock_launch(
            "child",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: child hello",
                &child_gate,
            )),
        )
        .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_new_agent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/child".to_owned()],
                prompt: "child hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let _ = expect_next_event(&mut fixture.client, "child NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "child AgentStart").await;
    child_gate.release_one();
    drop(child_reservation);
    expect_turn(&mut fixture.client, "mock backend response to: child hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions with parent/child failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList with parent/child").await;
    assert_eq!(
        list.sessions.len(),
        2,
        "expected two sessions in a single SessionList event"
    );

    let parent = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("parent"))
        .expect("missing parent session in SessionList");
    let child = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("child"))
        .expect("missing child session in SessionList");
    assert_eq!(
        child.parent_id.as_ref(),
        Some(&parent.id),
        "child session should point to parent session id",
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: parent.id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume without prompt failed");

    let env = expect_next_event(&mut fixture.client, "resumed parent NewAgent").await;
    let resumed_parent: NewAgentPayload =
        env.parse_payload().expect("parse resumed parent NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_parent.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed parent AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resumed parent AgentBootstrap");
    let start = bootstrap_agent_start(&payload);
    assert_eq!(start.agent_id, resumed_parent.agent_id);
    assert_bootstrap_tail_messages(&payload, &["parent hello"]);

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "resume without prompt should not start a turn",
    )
    .await;

    fixture
        .client
        .send_message(
            &resumed_parent.instance_stream,
            "after quiet resume".to_owned(),
        )
        .await
        .expect("send_message after quiet resume failed");

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after quiet resume",
    )
    .await;
}

#[tokio::test]
async fn session_project_id_persists_and_resume_can_override_it() {
    let mut fixture = Fixture::new().await;

    let project_a = create_project(
        &mut fixture.client,
        "Project A",
        vec!["/tmp/project-a".to_owned()],
    )
    .await;
    let project_b = create_project(
        &mut fixture.client,
        "Project B",
        vec!["/tmp/project-b".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("project-session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project_a.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&project_a),
                prompt: "session project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn project session failed");

    let env = expect_next_event(&mut fixture.client, "project session NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse project session NewAgent");
    assert_eq!(new_agent.project_id.as_ref(), Some(&project_a.id));

    let env = expect_next_event(&mut fixture.client, "project session AgentStart").await;
    let start: AgentStartPayload = env
        .parse_payload()
        .expect("parse project session AgentStart");
    assert_eq!(start.project_id.as_ref(), Some(&project_a.id));

    expect_turn(
        &mut fixture.client,
        "mock backend response to: session project",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after project spawn failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList after project spawn").await;
    assert_eq!(list.sessions.len(), 1);
    let session = &list.sessions[0];
    assert_eq!(session.project_id.as_ref(), Some(&project_a.id));

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-same-project".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("resume same".to_owned()),
            },
        })
        .await
        .expect("resume with stored project failed");

    let env = expect_next_event(&mut fixture.client, "resume same project NewAgent").await;
    let resumed_same: NewAgentPayload = env.parse_payload().expect("parse resumed same NewAgent");
    assert_eq!(resumed_same.project_id.as_ref(), Some(&project_a.id));
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_same.instance_stream,
        FrameKind::AgentBootstrap,
        "resume same project AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resume same AgentBootstrap");
    let resumed_same_start = bootstrap_agent_start(&payload);
    assert_eq!(resumed_same_start.project_id.as_ref(), Some(&project_a.id));
    assert_bootstrap_tail_messages(&payload, &["session project"]);
    expect_turn(&mut fixture.client, "mock backend response to: resume same").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-other-project".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project_b.id.clone()),
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("resume other".to_owned()),
            },
        })
        .await
        .expect("resume with overridden project failed");

    let env = expect_next_event(&mut fixture.client, "resume other project NewAgent").await;
    let resumed_other: NewAgentPayload = env.parse_payload().expect("parse resumed other NewAgent");
    assert_eq!(resumed_other.project_id.as_ref(), Some(&project_b.id));
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_other.instance_stream,
        FrameKind::AgentBootstrap,
        "resume other project AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resume other AgentBootstrap");
    let resumed_other_start = bootstrap_agent_start(&payload);
    assert_eq!(resumed_other_start.project_id.as_ref(), Some(&project_b.id));
    assert_bootstrap_tail_messages(&payload, &["session project", "resume same"]);
    expect_turn(
        &mut fixture.client,
        "mock backend response to: resume other",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after override failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList after override").await;
    assert_eq!(
        list.sessions.len(),
        1,
        "resume should still reuse one session"
    );
    assert_eq!(list.sessions[0].id, session.id);
    assert_eq!(list.sessions[0].project_id.as_ref(), Some(&project_b.id));
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: roots.into_iter().map(ProjectRootPath).collect(),
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create helper").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}

// Bug 6: Delete Session

#[tokio::test]
async fn delete_session_removes_it_from_list() {
    let mut fixture = Fixture::new().await;

    // Spawn an agent so a session gets recorded.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("to-delete".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/delete-session".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    // Confirm the session is present.
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, "initial SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one session before delete");
    let session_id = list.sessions[0].id.clone();

    // Delete the session — server will fan-out an updated SessionList automatically.
    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: session_id.clone(),
        })
        .await
        .expect("delete_session failed");

    for attempt in 0..3 {
        let list = wait_for_session_list(&mut fixture.client, "SessionList after delete").await;
        if list.sessions.is_empty() {
            return;
        }
        if attempt == 2 {
            panic!(
                "session list must be empty after delete, got {:?}",
                list.sessions
                    .iter()
                    .map(|s| s.id.0.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn delete_nonexistent_session_is_graceful() {
    let mut fixture = Fixture::new().await;

    // Delete a session that was never created — server must not crash and must
    // emit an updated (empty) SessionList.
    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: SessionId("nonexistent-session-id".to_owned()),
        })
        .await
        .expect("delete_session write failed");

    let list = wait_for_session_list(
        &mut fixture.client,
        "SessionList after deleting nonexistent session",
    )
    .await;
    assert!(
        list.sessions.is_empty(),
        "session list should be empty; deleting a nonexistent session must be a no-op"
    );
}

#[tokio::test]
async fn requested_compactions_keep_one_row_when_observed_after_completion() {
    use protocol::{
        CompactionTrigger, ContextCompactionNotifyPayload, ContextCompactionStatus,
        ContextCompactionTimelineStatus,
    };
    use server::backend::mock::MockGateHandle;

    let mut fixture = Fixture::new().await;
    let observation_release = MockGateHandle::new();
    let observation_sent = MockGateHandle::new();
    let busy_turn = MockGateHandle::new();
    let script = MockScript::one(MockTurn::text("ready"))
        .then(MockTurn::text("after idle compaction"))
        .then(MockTurn::gated_text("busy turn finished", &busy_turn))
        .then(MockTurn::text("after busy compaction"))
        .with_late_compaction_observation(&observation_release, &observation_sent);
    let agent = fixture
        .spawn_scripted("compaction correlation", script)
        .await;
    fixture.finish_turn(&agent).await;
    let mut operations = Vec::new();

    for busy in [false, true] {
        if busy {
            fixture
                .client
                .send_message(&agent.stream, "work before compacting".to_owned())
                .await
                .expect("start busy turn");
            busy_turn.wait_until_entered().await;
        }
        fixture
            .client
            .compact_agent(&agent.stream, protocol::AgentCompactPayload::default())
            .await
            .expect("request compaction");
        let mut saw_deferred = false;
        let terminal = loop {
            let frame = fixture::next_frame_matching_on(
                &mut fixture.client,
                "requested compaction status",
                |frame| {
                    frame.stream == agent.stream && frame.kind == FrameKind::ContextCompactionNotify
                },
            )
            .await;
            let notification: ContextCompactionNotifyPayload =
                frame.parse_payload().expect("compaction notification");
            match notification.status {
                ContextCompactionStatus::Deferred { stage } => {
                    assert_eq!(stage, protocol::CompactionStage::WaitingForIdle);
                    if busy && !saw_deferred {
                        busy_turn.release_one();
                    }
                    saw_deferred = true;
                }
                ContextCompactionStatus::Completed => break notification,
                ContextCompactionStatus::Failed { .. } => {
                    panic!("compaction failed: {notification:?}")
                }
                _ => {}
            }
        };
        // Admission publishes WaitingForIdle for every request before dispatch,
        // including requests received while idle.
        assert!(saw_deferred);
        operations.push(terminal.operation_id);
        observation_release.wait_until_entered().await;
        observation_release.release_one();
        observation_sent.wait_until_entered().await;
        observation_sent.release_one();
        fixture
            .client
            .send_message(&agent.stream, "continue after compaction".to_owned())
            .await
            .expect("send follow-up");
        fixture.finish_turn(&agent).await;

        let history = fetch_history_page(
            &mut fixture.client,
            &agent.stream,
            agent.new_agent.agent_id.clone(),
            None,
            100,
        )
        .await;
        let markers: Vec<_> = history
            .events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::ContextCompaction(marker) => Some(marker),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers.len(),
            operations.len(),
            "one persisted row per requested compaction: {markers:?}"
        );
        for operation in &operations {
            let matching: Vec<_> = markers
                .iter()
                .filter(|marker| marker.operation_id.as_ref() == Some(operation))
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].trigger, CompactionTrigger::UserRequested);
            assert_eq!(
                matching[0].status,
                ContextCompactionTimelineStatus::Completed
            );
        }
    }

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list compacted session");
    let sessions = wait_for_session_list(&mut fixture.client, "compacted session").await;
    assert_eq!(sessions.sessions.len(), 1);
    let session = &sessions.sessions[0];
    let (resumed, _) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("resumed compacted session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: None,
            },
        })
        .await;
    let history = fetch_history_page(
        &mut fixture.client,
        &resumed.stream,
        resumed.new_agent.agent_id.clone(),
        None,
        100,
    )
    .await;
    let markers: Vec<_> = history
        .events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::ContextCompaction(marker) => Some(marker),
            _ => None,
        })
        .collect();
    assert_eq!(
        markers.len(),
        operations.len(),
        "resume must preserve each compaction exactly once"
    );
    for operation in &operations {
        assert_eq!(
            markers
                .iter()
                .filter(|marker| marker.operation_id.as_ref() == Some(operation))
                .count(),
            1
        );
    }
}

/// Where the durable transcript journal for a session lives under the fixture.
fn transcript_journal_path(fixture: &Fixture, session_id: &SessionId) -> std::path::PathBuf {
    let safe = session_id
        .0
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    fixture
        .store_dir()
        .join("transcripts")
        .join(format!("{safe}.jsonl"))
}

fn journal_lines(path: &Path) -> Vec<String> {
    let contents = std::fs::read_to_string(path).expect("read transcript journal");
    contents
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

async fn wait_for_session_id(fixture: &mut Fixture, context: &str) -> SessionId {
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, context).await;
    assert_eq!(
        list.sessions.len(),
        1,
        "expected exactly one stored session"
    );
    list.sessions[0].id.clone()
}

/// Drive a turn to its end and count the agent errors seen along the way.
///
/// A latch that re-arms itself looks exactly like a working one until you count
/// the reports across a whole turn.
async fn expect_turn_counting_agent_errors(
    client: &mut client::Connection,
    expected_text: &str,
) -> usize {
    let mut errors = 0;
    loop {
        let env = fixture::next_frame_matching_on(client, expected_text, |env| {
            matches!(env.kind, FrameKind::AgentError | FrameKind::ChatEvent)
        })
        .await;
        if env.kind == FrameKind::AgentError {
            errors += 1;
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::StreamEnd(end) = event {
            assert!(
                end.message.content.contains(expected_text),
                "unexpected turn ended: {:?}",
                end.message.content
            );
            return errors;
        }
    }
}

async fn next_agent_error(client: &mut client::Connection, context: &str) -> AgentErrorPayload {
    fixture::next_frame_matching_on(client, context, |env| env.kind == FrameKind::AgentError)
        .await
        .parse_payload()
        .expect("parse AgentError")
}

/// A record torn by a full disk costs its own tail and nothing more.
///
/// `write_all` reports `ENOSPC` after committing part of a record, so the
/// journal ends in bytes that are not a record. Reading used to fail the whole
/// file over those bytes, which meant one bad write made the session
/// permanently unresumable — the history was still on disk, just unreachable.
/// Resume must still serve the intact prefix, and the damage must be cleared so
/// records written afterwards are readable too.
#[tokio::test]
async fn resume_serves_history_written_before_a_torn_transcript_record() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("torn".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);
    let intact_lines = journal_lines(&journal);
    assert!(
        !intact_lines.is_empty(),
        "the first turn should have written transcript records to {}",
        journal.display()
    );

    // Exactly what a full disk leaves behind: the leading bytes of a record,
    // with no terminator and no way to finish it. The session id inside it is a
    // sentinel, because every intact record opens with the same field and the
    // damage has to stay tellable apart from healthy bytes.
    let torn = br#"{"logical_session_id":"torn-by-a-full-disk","sequence":4"#;
    let torn_sentinel = b"torn-by-a-full-disk";
    let before_tear = std::fs::metadata(&journal).expect("stat journal").len();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal to tear it");
        file.write_all(torn).expect("write torn record");
    }

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");

    // The history written before the tear is what the tear must not cost.
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    // Appending behind torn bytes would bury every later record where no
    // reader can reach it, so the damage has to be gone.
    let contents = std::fs::read(&journal).expect("read journal after resume");
    assert!(
        !contents
            .windows(torn_sentinel.len())
            .any(|window| window == torn_sentinel),
        "the torn record should have been discarded from {}",
        journal.display()
    );
    assert_eq!(
        contents.last().copied(),
        Some(b'\n'),
        "every retained record must be terminated"
    );
    let repaired_lines = journal_lines(&journal);
    for line in &repaired_lines {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("every retained transcript record must parse");
    }
    // The repair may only ever remove from the end. Anything it rewrote or
    // dropped from the middle would be history lost to a fix meant to save it.
    assert_eq!(
        &repaired_lines[..intact_lines.len()],
        &intact_lines[..],
        "the records written before the tear must survive it unchanged"
    );
    assert!(
        repaired_lines.len() > intact_lines.len(),
        "records written after the repair should be readable: {} before, {} after",
        intact_lines.len(),
        repaired_lines.len()
    );
    assert!(
        std::fs::metadata(&journal)
            .expect("stat repaired journal")
            .len()
            >= before_tear,
        "the repair must keep the records that were already committed"
    );

    // A journal that parses but cannot be resumed from would be a repair in
    // name only, so read it back the way a user does: resume again and require
    // both turns, each exactly once.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed twice".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("second resume failed");

    let env = expect_next_event(&mut fixture.client, "second resume NewAgent").await;
    let again: NewAgentPayload = env.parse_payload().expect("parse second resume NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &again.instance_stream,
        FrameKind::AgentBootstrap,
        "second resume AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse second resume AgentBootstrap");
    let contents = bootstrap_message_contents(&payload);
    for turn in [
        "mock backend response to: hello",
        "mock backend response to: after resume",
    ] {
        let seen = contents
            .iter()
            .filter(|content| content.contains(turn))
            .count();
        assert_eq!(
            seen, 1,
            "expected {turn:?} exactly once after the repair, got {contents:?}"
        );
    }
    assert_eq!(
        contents.len(),
        2,
        "the repaired journal must replay both turns and nothing besides: {contents:?}"
    );
}

/// A transcript that stops being written says so.
///
/// The agent keeps running and the stream keeps updating when journaling
/// breaks, so the loss is invisible until a resume comes back short of the work
/// that was actually done. It has to reach the client while the turn is still
/// live, and it has to be non-fatal — the agent is fine, only its history is
/// not.
#[tokio::test]
async fn a_transcript_that_cannot_be_written_reports_a_non_fatal_error() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("unwritable".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);

    // A directory where the journal belongs fails every read and every write of
    // it, for any user, which is the durable stand-in for a disk that has
    // stopped accepting appends.
    // The actor may still be flushing the turn it just finished, so claim the
    // path until it stays claimed rather than racing it exactly once.
    let mut blocked = false;
    for _ in 0..100 {
        let _ = std::fs::remove_file(&journal);
        if std::fs::create_dir(&journal).is_ok() {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(blocked, "could not block {}", journal.display());

    fixture
        .client
        .send_message(
            &agent.instance_stream,
            "work that will not be saved".to_owned(),
        )
        .await
        .expect("send message failed");

    let error = next_agent_error(&mut fixture.client, "transcript persistence AgentError").await;
    assert_eq!(error.agent_id, agent.agent_id);
    assert!(
        !error.fatal,
        "a broken transcript must not be reported as a fatal agent error"
    );
    assert!(
        error.message.contains("transcript"),
        "the error should name what broke, got {:?}",
        error.message
    );

    // The agent itself is unaffected, which is precisely why the failure needs
    // announcing rather than inferring from the stream.
    let extra = expect_turn_counting_agent_errors(
        &mut fixture.client,
        "mock backend response to: work that will not be saved",
    )
    .await;
    assert_eq!(
        extra, 0,
        "one outage must report once, not once per event in the turn"
    );

    // A second turn in the same outage is still the same outage.
    fixture
        .client
        .send_message(&agent.instance_stream, "still not saved".to_owned())
        .await
        .expect("send second message failed");
    let extra = expect_turn_counting_agent_errors(
        &mut fixture.client,
        "mock backend response to: still not saved",
    )
    .await;
    assert_eq!(
        extra, 0,
        "a continuing outage must not re-report itself every turn"
    );
}

/// A record this build cannot read is not a record it may delete.
///
/// The repair exists for bytes no writer ever finished. A complete,
/// newline-terminated record that fails to deserialize is a different thing --
/// most likely written by a build that knows an event this one does not -- and
/// the journal is its only copy. Reading stops there, because guessing past it
/// would mis-sequence everything after, but the bytes have to stay on disk.
#[tokio::test]
async fn a_record_this_build_cannot_read_is_kept_on_disk() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("unknown schema".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);
    let intact_lines = journal_lines(&journal);

    // Well-formed JSON, properly terminated, carrying an event this build has
    // no variant for. A newer Tyde writing a newer event looks exactly so.
    let unknown = format!(
        r#"{{"logical_session_id":"{}","sequence":9999,"event_id":"from-a-newer-build","visibility":"visible","event":{{"type":"an_event_from_the_future","payload":{{"kept":true}}}},"timestamp_ms":1}}"#,
        session_id.0
    );
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal");
        writeln!(file, "{unknown}").expect("write unknown record");
    }
    let after_write = std::fs::read_to_string(&journal).expect("read journal");

    // Resuming reads the journal, and reading is what used to truncate it.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");

    // The prefix is still served: an unreadable record costs what follows it,
    // not what precedes it.
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    let contents = std::fs::read_to_string(&journal).expect("read journal after resume");
    assert!(
        contents.contains("from-a-newer-build"),
        "a record this build cannot read must not be deleted from {}",
        journal.display()
    );
    assert!(
        contents.starts_with(&after_write),
        "the journal may only ever grow here, never be rewritten"
    );
    assert_eq!(
        journal_lines(&journal)[..intact_lines.len()],
        intact_lines[..],
        "records written before the unreadable one must be untouched"
    );
}
