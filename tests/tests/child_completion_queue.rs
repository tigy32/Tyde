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

async fn expect_turn_on_stream_contains(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    loop {
        let env = raw_next(client, "TypingStatusChanged(true)").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::TypingStatusChanged(true)) {
            break;
        }
    }

    loop {
        let env = raw_next(client, "StreamStart").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::StreamStart(..)) {
            break;
        }
    }

    loop {
        let env = raw_next(client, "StreamDelta").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::StreamDelta(delta) = event {
            assert!(
                delta.text.contains(expected_text),
                "expected delta on {stream} to contain {expected_text:?}, got {:?}",
                delta.text
            );
            break;
        }
    }

    loop {
        let env = raw_next(client, "StreamEnd").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(..)) {
            break;
        }
    }

    loop {
        let env = raw_next(client, "TypingStatusChanged(false)").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::TypingStatusChanged(false)) {
            return;
        }
    }
}

async fn expect_queued_messages_with_count(
    client: &mut client::Connection,
    count: usize,
    context: &str,
) -> QueuedMessagesPayload {
    loop {
        let env = raw_next(client, context).await;
        if env.kind != FrameKind::QueuedMessages {
            continue;
        }
        let payload: QueuedMessagesPayload = env.parse_payload().expect("parse QueuedMessages");
        if payload.messages.len() == count {
            return payload;
        }
    }
}

fn child_notice_text(
    child_name: &str,
    child_id: &AgentId,
    outcome: &str,
    message_text: &str,
) -> String {
    format!(
        "[TYDE CHILD AGENT UPDATE]\nThis is an automatic system-generated child completion notice, not a user instruction.\nChild name: {child_name}\nChild id: {child_id}\nChild state: idle\nChild outcome: {outcome}\n\nChild message:\n{message_text}\n[END TYDE CHILD AGENT UPDATE]"
    )
}

fn mock_turn_text(prompt: &str) -> String {
    format!("[startup_mcp_servers: tyde-agent-control(http)] mock backend response to: {prompt}")
}

#[tokio::test]
async fn child_completion_is_enqueued_on_parent_queue() {
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

    let snapshot =
        expect_queued_messages_with_count(&mut fixture.client, 1, "queued child completion notice")
            .await;
    assert_eq!(
        snapshot.messages[0].message,
        child_notice_text(
            "child-complete",
            &child_new.agent_id,
            "completed",
            &mock_turn_text("child completed"),
        )
    );
}

#[tokio::test]
async fn child_cancellation_is_enqueued_on_parent_queue() {
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

    let snapshot = expect_queued_messages_with_count(
        &mut fixture.client,
        1,
        "queued child cancellation notice",
    )
    .await;
    assert_eq!(
        snapshot.messages[0].message,
        child_notice_text(
            "child-cancelled",
            &child_new.agent_id,
            "cancelled",
            "mock backend cancelled: __mock_cancel__ child cancelled",
        )
    );
}

#[tokio::test]
async fn two_child_completions_are_enqueued_in_arrival_order() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-two-children",
        "__mock_slow__ parent busy",
        None,
    )
    .await;
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    let (child_a_new, _) = spawn_agent(
        &mut fixture.client,
        "child-a",
        "first child",
        Some(parent_new.agent_id.clone()),
    )
    .await;
    let (child_b_new, _) = spawn_agent(
        &mut fixture.client,
        "child-b",
        "second child",
        Some(parent_new.agent_id.clone()),
    )
    .await;

    let snapshot = expect_queued_messages_with_count(
        &mut fixture.client,
        2,
        "two queued child completion notices",
    )
    .await;
    assert_eq!(
        snapshot.messages[0].message,
        child_notice_text(
            "child-a",
            &child_a_new.agent_id,
            "completed",
            &mock_turn_text("first child"),
        )
    );
    assert_eq!(
        snapshot.messages[1].message,
        child_notice_text(
            "child-b",
            &child_b_new.agent_id,
            "completed",
            &mock_turn_text("second child"),
        )
    );
}

#[tokio::test]
async fn backend_native_child_does_not_enqueue_completion_notice() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    // Parent stays busy long enough for any child-completion notice to be
    // enqueued if the server were (buggily) going to emit one.
    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-native",
        "__mock_slow__ __mock_spawn_native_child__ parent busy",
        None,
    )
    .await;
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    // Wait long enough for the backend-native child to have completed. The
    // parent's slow turn (MOCK_SLOW_SLEEP_MS) gives headroom for this.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), fixture.client.next_event()).await {
            Ok(Ok(Some(env))) => {
                assert_ne!(
                    env.kind,
                    FrameKind::QueuedMessages,
                    "backend-native child completion must not enqueue a notice on the parent queue"
                );
            }
            Ok(Ok(None)) => panic!("connection closed unexpectedly"),
            Ok(Err(err)) => panic!("next_event failed: {err:?}"),
            Err(_) => {}
        }
    }
}

#[tokio::test]
async fn idle_parent_immediately_reenters_turn_for_child_completion_notice() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let (parent_new, _) =
        spawn_agent(&mut fixture.client, "idle-parent", "parent idle", None).await;
    expect_turn_on_stream_contains(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent idle",
    )
    .await;

    let (child_new, _) = spawn_agent(
        &mut fixture.client,
        "idle-parent-child",
        "child auto resume",
        Some(parent_new.agent_id.clone()),
    )
    .await;

    let expected_notice = child_notice_text(
        "idle-parent-child",
        &child_new.agent_id,
        "completed",
        &mock_turn_text("child auto resume"),
    );
    expect_turn_on_stream_contains(
        &mut fixture.client,
        &parent_new.instance_stream,
        &expected_notice,
    )
    .await;
}
