mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentId, AgentStartPayload, BackendKind, ChatEvent,
    Envelope, FrameKind, NewAgentPayload, QueuedMessagesPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

async fn observe_frames_for(
    client: &mut client::Connection,
    duration: Duration,
    mut check: impl FnMut(&Envelope),
) {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), client.next_event()).await {
            Ok(Ok(Some(env))) => check(&env),
            Ok(Ok(None)) => panic!("connection closed unexpectedly"),
            Ok(Err(err)) => panic!("next_event failed: {err:?}"),
            Err(_) => {}
        }
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let mut new_agent = None;
    fixture::next_frame_matching_on(client, "NewAgent", |env| {
        if env.kind != FrameKind::NewAgent {
            return false;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
        if payload.name == name {
            new_agent = Some(payload);
            true
        } else {
            false
        }
    })
    .await;
    let new_agent = new_agent.expect("matched NewAgent");

    let mut start: Option<AgentStartPayload> = None;
    fixture::next_frame_matching_on(client, "AgentStart", |env| {
        if env.stream != new_agent.instance_stream {
            return false;
        }
        match env.kind {
            FrameKind::AgentStart => {
                start = Some(env.parse_payload().expect("parse AgentStartPayload"));
                true
            }
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse AgentBootstrapPayload");
                match bootstrap.events.into_iter().find_map(|event| match event {
                    AgentBootstrapEvent::AgentStart(payload) => Some(payload),
                    _ => None,
                }) {
                    Some(bootstrapped) => {
                        start = Some(bootstrapped);
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    })
    .await;
    let start = start.expect("matched AgentStart");

    (new_agent, start)
}

async fn wait_for_typing_true(client: &mut client::Connection, stream: &StreamPath) {
    fixture::next_frame_matching_on(client, "TypingStatusChanged(true)", |env| {
        if env.stream != *stream {
            return false;
        }
        match env.kind {
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse AgentBootstrapPayload");
                bootstrap.events.into_iter().any(|event| {
                    matches!(
                        event,
                        AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(true))
                    )
                })
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                matches!(event, ChatEvent::TypingStatusChanged(true))
            }
            _ => false,
        }
    })
    .await;
}

fn assert_no_nonempty_parent_queue(env: &Envelope, parent_stream: &StreamPath) {
    if env.kind == FrameKind::AgentBootstrap && env.stream == *parent_stream {
        let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
        for event in payload.events {
            if let AgentBootstrapEvent::QueuedMessages(payload) = event {
                assert!(
                    payload.messages.is_empty(),
                    "child completion must not enqueue messages on parent queue: {:?}",
                    payload.messages
                );
            }
        }
    }
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

    fixture::next_frame_matching_on(client, "child completed turn", |env| {
        assert_no_nonempty_parent_queue(env, parent_stream);
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            return false;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::StreamDelta(delta) => {
                saw_expected_text |= delta.text.contains(expected_text);
                false
            }
            ChatEvent::StreamEnd(data) => {
                saw_expected_text |= data.message.content.contains(expected_text);
                saw_stream_end = true;
                false
            }
            ChatEvent::TypingStatusChanged(false) if saw_stream_end => {
                assert!(
                    saw_expected_text,
                    "expected child turn on {stream} to contain {expected_text:?}"
                );
                true
            }
            _ => false,
        }
    })
    .await;
}

async fn expect_cancelled_turn_without_parent_queue(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
    parent_stream: &StreamPath,
) {
    let mut saw_cancel = false;

    fixture::next_frame_matching_on(client, "child cancelled turn", |env| {
        assert_no_nonempty_parent_queue(env, parent_stream);
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            return false;
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
                false
            }
            ChatEvent::TypingStatusChanged(false) if saw_cancel => true,
            _ => false,
        }
    })
    .await;
}

async fn assert_no_parent_reentry(
    client: &mut client::Connection,
    parent_stream: &StreamPath,
    duration: Duration,
) {
    observe_frames_for(client, duration, |env| {
        assert_no_nonempty_parent_queue(env, parent_stream);
        assert!(
            !(env.kind == FrameKind::ChatEvent && env.stream == *parent_stream),
            "child completion must not re-enter the parent turn, got {env:?}"
        );
    })
    .await;
}

fn mock_turn_text(prompt: &str) -> String {
    format!(
        "[startup_mcp_servers: tyde-agent-control(http), tyde-agent-await(http)] mock backend response to: {prompt}"
    )
}

#[tokio::test]
async fn child_completion_does_not_enqueue_on_parent_queue() {
    let mut fixture = Fixture::new().await;

    let parent_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-busy",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: parent busy",
                &parent_gate,
            )),
        )
        .await;
    let (parent_new, _) =
        spawn_agent(&mut fixture.client, "parent-busy", "parent busy", None).await;
    drop(reservation);
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
    let mut fixture = Fixture::new().await;

    let parent_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-cancel-busy",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: parent busy",
                &parent_gate,
            )),
        )
        .await;
    let (parent_new, _) = spawn_agent(
        &mut fixture.client,
        "parent-cancel-busy",
        "parent busy",
        None,
    )
    .await;
    drop(reservation);
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;

    let child_reservation = fixture
        .reserve_next_mock_launch(
            "child-cancelled",
            MockScript::one(MockTurn::cancelled(
                "mock backend cancelled: child cancelled",
            )),
        )
        .await;
    let (child_new, _) = spawn_agent(
        &mut fixture.client,
        "child-cancelled",
        "child cancelled",
        Some(parent_new.agent_id.clone()),
    )
    .await;
    drop(child_reservation);

    expect_cancelled_turn_without_parent_queue(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend cancelled: child cancelled",
        &parent_new.instance_stream,
    )
    .await;
}

#[tokio::test]
async fn backend_native_child_does_not_enqueue_completion_notice() {
    let mut fixture = Fixture::new().await;

    // The backend-native child spawns and completes MID-TURN — after the
    // parent's stream end but before the gate and the trailing idle — so the
    // whole child lifecycle happens while the parent is provably busy. Gate
    // entry is causally after the child effect ran, and the gate is released
    // only after the observation window, so every parent-queue assertion
    // below runs against a parent that is still mid-turn while its native
    // child has actually completed.
    let parent_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-native",
            MockScript::one(MockTurn::gated_text_with_busy_native_child(
                "mock backend response to: parent busy",
                "mock-native-child",
                "child completed",
                &parent_gate,
            )),
        )
        .await;
    let (parent_new, _) =
        spawn_agent(&mut fixture.client, "parent-native", "parent busy", None).await;
    drop(reservation);
    wait_for_typing_true(&mut fixture.client, &parent_new.instance_stream).await;
    parent_gate.wait_until_entered().await;

    // Positive child-lifecycle evidence during the busy window: the native
    // child agent appears and runs its full completed turn, with the parent
    // queue asserted empty on every frame consumed along the way.
    let mut native_child = None;
    fixture::next_frame_matching_on(&mut fixture.client, "native child NewAgent", |env| {
        assert_no_nonempty_parent_queue(env, &parent_new.instance_stream);
        if env.kind != FrameKind::NewAgent {
            return false;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
        if payload.name == "mock-native-child" {
            native_child = Some(payload);
            true
        } else {
            false
        }
    })
    .await;
    let native_child = native_child.expect("matched native child NewAgent");
    expect_completed_turn_without_parent_queue(
        &mut fixture.client,
        &native_child.instance_stream,
        "mock native child response to: child completed",
        &parent_new.instance_stream,
    )
    .await;

    observe_frames_for(&mut fixture.client, Duration::from_secs(3), |env| {
        assert_no_nonempty_parent_queue(env, &parent_new.instance_stream);
    })
    .await;
    drop(parent_gate);
}

#[tokio::test]
async fn idle_parent_does_not_reenter_turn_for_child_completion() {
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
