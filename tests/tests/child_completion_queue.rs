mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentId, AgentStartPayload, BackendKind, ChatEvent, Envelope, FrameKind, NewAgentPayload,
    QueuedMessagesPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
};

async fn raw_next(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn spawn_agent(
    client: &mut client::Connection,
    name: &str,
    prompt: &str,
    parent_agent_id: Option<AgentId>,
) -> (NewAgentPayload, AgentStartPayload) {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/child-completion-queue".to_owned()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let new_agent = loop {
        let env = raw_next(client, "NewAgent").await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
        if payload.name == name {
            break payload;
        }
    };

    let start = loop {
        let env = raw_next(client, "AgentStart").await;
        if env.kind != FrameKind::AgentStart || env.stream != new_agent.instance_stream {
            continue;
        }
        break env.parse_payload().expect("parse AgentStartPayload");
    };

    (new_agent, start)
}

async fn wait_for_typing_true(client: &mut client::Connection, stream: &StreamPath) {
    loop {
        let env = raw_next(client, "TypingStatusChanged(true)").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::TypingStatusChanged(true)) {
            return;
        }
    }
}

fn assert_no_nonempty_parent_queue(env: &Envelope, parent_stream: &StreamPath) {
    if env.kind != FrameKind::QueuedMessages || env.stream != *parent_stream {
        return;
    }
    let payload: QueuedMessagesPayload = env.parse_payload().expect("parse QueuedMessages");
    assert!(
        payload.messages.is_empty(),
        "child completion must not enqueue messages on parent queue: {:?}",
        payload.messages
    );
}

async fn expect_completed_turn_without_parent_queue(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
    parent_stream: &StreamPath,
) {
    let mut saw_expected_text = false;
    let mut saw_stream_end = false;

    loop {
        let env = raw_next(client, "child completed turn").await;
        assert_no_nonempty_parent_queue(&env, parent_stream);
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::StreamDelta(delta) => {
                saw_expected_text |= delta.text.contains(expected_text);
            }
            ChatEvent::StreamEnd(data) => {
                saw_expected_text |= data.message.content.contains(expected_text);
                saw_stream_end = true;
            }
            ChatEvent::TypingStatusChanged(false) if saw_stream_end => {
                assert!(
                    saw_expected_text,
                    "expected child turn on {stream} to contain {expected_text:?}"
                );
                return;
            }
            _ => {}
        }
    }
}

async fn expect_cancelled_turn_without_parent_queue(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
    parent_stream: &StreamPath,
) {
    let mut saw_cancel = false;

    loop {
        let env = raw_next(client, "child cancelled turn").await;
        assert_no_nonempty_parent_queue(&env, parent_stream);
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::OperationCancelled(data) => {
                assert!(
                    data.message.contains(expected_text),
                    "expected child cancellation to contain {expected_text:?}, got {:?}",
                    data.message
                );
                saw_cancel = true;
            }
            ChatEvent::TypingStatusChanged(false) if saw_cancel => return,
            _ => {}
        }
    }
}

async fn assert_no_parent_reentry(
    client: &mut client::Connection,
    parent_stream: &StreamPath,
    duration: Duration,
) {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), client.next_event()).await {
            Ok(Ok(Some(env))) => {
                assert_no_nonempty_parent_queue(&env, parent_stream);
                assert!(
                    !(env.kind == FrameKind::ChatEvent && env.stream == *parent_stream),
                    "child completion must not re-enter the parent turn, got {env:?}"
                );
            }
            Ok(Ok(None)) => panic!("connection closed unexpectedly"),
            Ok(Err(err)) => panic!("next_event failed: {err:?}"),
            Err(_) => {}
        }
    }
}

fn mock_turn_text(prompt: &str) -> String {
    format!("[startup_mcp_servers: tyde-agent-control(http)] mock backend response to: {prompt}")
}

#[tokio::test]
async fn child_completion_does_not_enqueue_on_parent_queue() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-busy",
        "__mock_slow__ parent busy",
        None,
    )
    .await;
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    let (child_new, _) = spawn_agent(
        &mut fixture.client,
        "child-complete",
        "child completed",
        Some(parent_new.agent_id.clone()),
    )
    .await;

    expect_completed_turn_without_parent_queue(
        &mut fixture.client,
        &child_new.instance_stream,
        &mock_turn_text("child completed"),
        &parent_new.instance_stream,
    )
    .await;
}

#[tokio::test]
async fn child_cancellation_does_not_enqueue_on_parent_queue() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-cancel-busy",
        "__mock_slow__ parent busy",
        None,
    )
    .await;
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    let (child_new, _) = spawn_agent(
        &mut fixture.client,
        "child-cancelled",
        "__mock_cancel__ child cancelled",
        Some(parent_new.agent_id.clone()),
    )
    .await;

    expect_cancelled_turn_without_parent_queue(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend cancelled: __mock_cancel__ child cancelled",
        &parent_new.instance_stream,
    )
    .await;
}

#[tokio::test]
async fn backend_native_child_does_not_enqueue_completion_notice() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-native",
        "__mock_slow__ __mock_spawn_native_child__ parent busy",
        None,
    )
    .await;
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), fixture.client.next_event()).await {
            Ok(Ok(Some(env))) => assert_no_nonempty_parent_queue(&env, &parent_new.instance_stream),
            Ok(Ok(None)) => panic!("connection closed unexpectedly"),
            Ok(Err(err)) => panic!("next_event failed: {err:?}"),
            Err(_) => {}
        }
    }
}

#[tokio::test]
async fn idle_parent_does_not_reenter_turn_for_child_completion() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) =
        spawn_agent(&mut fixture.client, "idle-parent", "parent idle", None).await;
    expect_completed_turn_without_parent_queue(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent idle",
        &parent_new.instance_stream,
    )
    .await;

    let (child_new, _) = spawn_agent(
        &mut fixture.client,
        "idle-parent-child",
        "child stays separate",
        Some(parent_new.agent_id.clone()),
    )
    .await;

    expect_completed_turn_without_parent_queue(
        &mut fixture.client,
        &child_new.instance_stream,
        &mock_turn_text("child stays separate"),
        &parent_new.instance_stream,
    )
    .await;
    assert_no_parent_reentry(
        &mut fixture.client,
        &parent_new.instance_stream,
        Duration::from_millis(500),
    )
    .await;
}
