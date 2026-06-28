mod fixture;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentStartPayload, BackendKind, ChatEvent,
    DeleteSessionPayload, Envelope, FetchSessionHistoryPayload, FrameKind, ListSessionsPayload,
    NewAgentPayload, Project, ProjectCreatePayload, ProjectNotifyPayload, ProjectRootPath,
    SessionHistoryPayload, SessionId, SessionListPayload, SpawnAgentParams, SpawnAgentPayload,
    StreamPath,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const MOCK_AGENT_CONTROL_AWAIT_SENTINEL: &str = "__mock_agent_control_await__";
const MOCK_HOLD_UNTIL_INTERRUPT_SENTINEL: &str = "__mock_hold_until_interrupt__";
const MOCK_CLOSE_RESUME_BEFORE_BARRIER_SENTINEL: &str = "__mock_close_resume_before_barrier__";

static PENDING_BOOTSTRAP_EVENTS: OnceLock<Mutex<HashMap<StreamPath, VecDeque<Envelope>>>> =
    OnceLock::new();

fn pending_bootstrap_events() -> &'static Mutex<HashMap<StreamPath, VecDeque<Envelope>>> {
    PENDING_BOOTSTRAP_EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client_key(client: &client::Connection) -> StreamPath {
    let mut host_streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let host_stream = host_streams
        .next()
        .cloned()
        .expect("missing host stream for test connection");
    assert!(
        host_streams.next().is_none(),
        "test connection has multiple host streams"
    );
    host_stream
}

fn pop_pending_bootstrap_event(client: &mut client::Connection) -> Option<Envelope> {
    let key = client_key(client);
    let mut pending = pending_bootstrap_events()
        .lock()
        .expect("pending bootstrap event lock poisoned");
    let queue = pending.get_mut(&key)?;
    let event = queue.pop_front();
    if queue.is_empty() {
        pending.remove(&key);
    }
    event
}

fn push_bootstrap_events(
    client: &mut client::Connection,
    events: impl IntoIterator<Item = Envelope>,
) {
    let mut events = events.into_iter().collect::<VecDeque<_>>();
    if events.is_empty() {
        return;
    }
    let key = client_key(client);
    let mut pending = pending_bootstrap_events()
        .lock()
        .expect("pending bootstrap event lock poisoned");
    pending.entry(key).or_default().append(&mut events);
}

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        if let Some(env) = pop_pending_bootstrap_event(client) {
            return env;
        }
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let payload: AgentBootstrapPayload =
                env.parse_payload().expect("parse AgentBootstrapPayload");
            let events = payload.events.into_iter().filter_map(|event| match event {
                AgentBootstrapEvent::AgentStart(payload) => Some(
                    Envelope::from_payload(
                        env.stream.clone(),
                        FrameKind::AgentStart,
                        env.seq,
                        &payload,
                    )
                    .expect("serialize AgentStart"),
                ),
                AgentBootstrapEvent::ChatEvent(payload) => Some(
                    Envelope::from_payload(
                        env.stream.clone(),
                        FrameKind::ChatEvent,
                        env.seq,
                        &payload,
                    )
                    .expect("serialize ChatEvent"),
                ),
                _ => None,
            });
            push_bootstrap_events(client, events);
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::AgentActivityStats
        ) {
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
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.stream != *stream {
            continue;
        }
        if env.kind == kind {
            return env;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
        ) {
            continue;
        }
        panic!(
            "wait for {kind} on {stream} during {context} received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    }
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

fn assert_history_page(
    page: &SessionHistoryPayload,
    expected_newest_first: &[&str],
    expected_has_more_before: bool,
) {
    assert_eq!(
        page.events.len(),
        expected_newest_first.len(),
        "unexpected SessionHistory event count: {:?}",
        page.events
    );
    for (event, expected) in page.events.iter().zip(expected_newest_first) {
        let ChatEvent::MessageAdded(message) = event else {
            panic!("expected MessageAdded history event, got {event:?}");
        };
        assert!(
            message.content.contains(expected),
            "history message content {:?} did not contain {expected:?}",
            message.content
        );
    }
    assert_eq!(page.has_more_before, expected_has_more_before);
}

async fn wait_for_session_list(
    client: &mut client::Connection,
    context: &str,
) -> SessionListPayload {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::NewAgent
                | FrameKind::AgentStart
                | FrameKind::AgentError
                | FrameKind::ChatEvent
        ) {
            continue;
        }
        if env.kind == FrameKind::SessionList {
            return env
                .parse_payload()
                .expect("failed to parse SessionListPayload");
        }
        panic!(
            "wait_for_session_list({context}) received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    }
}

async fn expect_turn(client: &mut client::Connection, expected_text: &str) {
    let env = expect_next_event(client, "TypingStatusChanged(true) or StreamStart").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    let delta = match event {
        ChatEvent::TypingStatusChanged(true) => {
            let env = expect_next_event(client, "StreamStart").await;
            assert_eq!(env.kind, FrameKind::ChatEvent);
            let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
            match event {
                ChatEvent::StreamStart(_) => {
                    let env = expect_next_event(client, "StreamDelta").await;
                    assert_eq!(env.kind, FrameKind::ChatEvent);
                    env.parse_payload().expect("failed to parse ChatEvent")
                }
                delta @ ChatEvent::StreamDelta(_) => delta,
                other => panic!("expected StreamStart or StreamDelta, got {other:?}"),
            }
        }
        ChatEvent::StreamStart(_) => {
            let env = expect_next_event(client, "StreamDelta").await;
            assert_eq!(env.kind, FrameKind::ChatEvent);
            env.parse_payload().expect("failed to parse ChatEvent")
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

    let env = expect_next_event(client, "StreamEnd").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let env = expect_next_event(client, "TypingStatusChanged(false)").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if fixture::is_builtin_team_custom_agent_notify(&env)
                    || matches!(
                        env.kind,
                        FrameKind::HostSettings
                            | FrameKind::SessionSchemas
                            | FrameKind::BackendSetup
                            | FrameKind::QueuedMessages
                            | FrameKind::SessionSettings
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::SessionList
                            | FrameKind::WorkflowNotify
                            | FrameKind::AgentsViewPreferencesNotify
                    ) =>
            {
                continue;
            }
            Ok(Ok(Some(env))) => panic!(
                "unexpected event before {context}: kind={} stream={}",
                env.kind, env.stream
            ),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
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
    assert_history_page(&first_page, &["history 39", "history 38"], true);
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
    assert_history_page(&second_page, &["history 37", "history 36"], true);
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
        .rev()
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
            "prior 35", "prior 34", "prior 33", "prior 32", "prior 31", "prior 30", "prior 29",
            "prior 28", "prior 27", "prior 26",
        ],
        true,
    );
    assert!(
        first_page.events.iter().all(|event| {
            !matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if message.content.contains("new visible message")
            )
        }),
        "first history fetch must not duplicate live rows: {:?}",
        first_page.events
    );
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
                prompt: MOCK_CLOSE_RESUME_BEFORE_BARRIER_SENTINEL.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
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
        "mock backend response to: __mock_close_resume_before_barrier__",
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
async fn tycode_resume_replay_history_is_ingested_without_live_broadcast() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("tycode-resume-no-leak-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/tycode-resume-no-leak".to_owned()],
                prompt: "tycode original history".to_owned(),
                images: None,
                backend_kind: BackendKind::Tycode,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn tycode resume no-leak source failed");

    let _ = expect_next_event(&mut fixture.client, "tycode resume source NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "tycode resume source AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: tycode original history",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before tycode resume failed");
    let list = wait_for_session_list(
        &mut fixture.client,
        "SessionList before tycode no-leak resume",
    )
    .await;
    let session_id = list
        .sessions
        .first()
        .expect("expected tycode source session")
        .id
        .clone();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("tycode-resume-no-leak".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume tycode no-leak agent failed");

    let env = expect_next_event(&mut fixture.client, "tycode resume no-leak NewAgent").await;
    let resumed: NewAgentPayload = env
        .parse_payload()
        .expect("parse tycode resume no-leak NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "tycode resume no-leak AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &["tycode original history"]);

    expect_no_chat_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        Duration::from_millis(150),
        "new turn after quiet tycode resume",
    )
    .await;

    fixture
        .client
        .send_message(
            &resumed.instance_stream,
            "new tycode turn after resume".to_owned(),
        )
        .await
        .expect("send after tycode no-leak resume failed");
    expect_turn_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        "mock backend response to: new tycode turn after resume",
    )
    .await;
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

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("active-history-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/active-history-child".to_owned()],
                prompt: format!("{MOCK_HOLD_UNTIL_INTERRUPT_SENTINEL} child active"),
                images: None,
                backend_kind: BackendKind::Claude,
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

    fixture
        .client
        .send_message(
            &parent.instance_stream,
            format!("{MOCK_AGENT_CONTROL_AWAIT_SENTINEL} {}", child.agent_id.0),
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
    assert!(
        payload.events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(_))
        )),
        "active StreamStart should be replayed in AgentBootstrap: {:?}",
        payload.events
    );
    assert!(
        payload.events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(request))
                if request.tool_name == "tyde_await_agents"
        )),
        "active tool request should be replayed in AgentBootstrap: {:?}",
        payload.events
    );
    assert!(
        payload.events.iter().all(|event| !matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(_))
        )),
        "active bootstrap should not synthesize a completed prior turn: {:?}",
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
    expect_turn(
        &mut fixture.client,
        "mock backend response to: parent hello",
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
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let _ = expect_next_event(&mut fixture.client, "child NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "child AgentStart").await;
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
