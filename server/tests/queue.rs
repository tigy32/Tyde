mod fixture;

use fixture::Fixture;
use protocol::{
    AgentErrorPayload, CancelQueuedMessagePayload, ChatEvent, EditQueuedMessagePayload, FrameKind,
    ImageData, MessageOrigin, QueuedMessageId, QueuedMessagesPayload, SendMessagePayload,
    SendMessageToolResponse, SendQueuedMessageNowPayload, StreamPath,
};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

async fn assert_queue_not_emptied_before_next_typing_true(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    context: &str,
) {
    fixture::next_frame_matching_on(client, context, |env| {
        if env.stream != *agent_stream {
            return false;
        }
        match env.kind {
            FrameKind::QueuedMessages => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("parse QueuedMessagesPayload");
                assert!(
                    !payload.messages.is_empty(),
                    "stale TypingStatusChanged(false) drained the queue before the next turn became busy"
                );
                false
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

#[tokio::test(start_paused = true)]
async fn exit_plan_mode_tool_response_resumes_and_drains_queue() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "exit-plan-mode",
            MockScript::one(MockTurn::exit_plan_request(
                "epm-queue-drain",
                "# Plan\n\nApprove the mock plan.",
            ))
            .then(MockTurn::text(
                "mock backend response to: mock ExitPlanMode approved",
            ))
            .then(MockTurn::text(
                "mock backend response to: queued while ExitPlanMode is pending",
            )),
        )
        .await;

    let request = fixture
        .expect_paused_tool_request(&agent, "ExitPlanMode")
        .await;
    let protocol::ToolRequestType::ExitPlanMode { plan, plan_path } = &request.tool_type else {
        panic!("expected ExitPlanMode tool request");
    };
    assert_eq!(plan.as_deref(), Some("# Plan\n\nApprove the mock plan."));
    assert_eq!(plan_path.as_deref(), Some("/tmp/mock/mock-plan.md"));

    fixture
        .client
        .send_message(
            &agent.stream,
            "queued while ExitPlanMode is pending".to_owned(),
        )
        .await
        .expect("send queued message while waiting for ExitPlanMode");
    let queued = fixture.expect_queued_messages(&agent, 1).await;
    assert_eq!(
        queued.messages[0].message,
        "queued while ExitPlanMode is pending"
    );

    fixture.approve_exit_plan_mode(&agent, &request).await;

    let approval_turn = fixture.finish_turn(&agent).await;
    approval_turn.assert_tool_completed(&request.tool_call_id);
    approval_turn.assert_stream_end_contains("mock ExitPlanMode approved");

    let drain_turn = fixture.finish_turn(&agent).await;
    drain_turn.assert_stream_end_contains("mock backend response to: queued while ExitPlanMode");
    assert!(
        approval_turn.saw_queue_drained() || drain_turn.saw_queue_drained(),
        "queue never reported empty after ExitPlanMode approval"
    );
}

#[tokio::test(start_paused = true)]
async fn stale_tool_response_while_idle_does_not_wedge_follow_up() {
    let mut fixture = Fixture::new().await;
    let agent = fixture.spawn("stale-tool-response", "initial").await;

    fixture
        .next_chat_event_matching(&agent, "initial mock response", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end)
                    if end.message.content.contains("mock backend response to: initial")
            )
        })
        .await;
    fixture
        .next_chat_event_matching(&agent, "initial idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;

    fixture
        .client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: String::new(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                    tool_call_id: "stale-tool-call".to_owned(),
                    decision: protocol::ExitPlanModeDecision::Approve,
                    feedback: None,
                }),
            },
        )
        .await
        .expect("send stale ExitPlanMode response");
    fixture
        .next_chat_event_matching(&agent, "stale response backend error", |event| {
            matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, protocol::MessageSender::Error)
                        && message.content.contains("No matching pending tool request")
            )
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "after stale response".to_owned())
        .await
        .expect("send follow-up after stale tool response");
    fixture
        .next_chat_event_matching(&agent, "follow-up after stale tool response", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end)
                    if end
                        .message
                        .content
                        .contains("mock backend response to: after stale response")
            )
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn queue_while_busy_snapshot_grows() {
    let mut fixture = Fixture::new().await;

    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-grow",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: hello",
                &gate,
            )),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "queued A".to_owned())
        .await
        .expect("send_message A failed");

    let snapshot1 = fixture.expect_queued_messages(&agent, 1).await;
    assert_eq!(snapshot1.messages.len(), 1);
    assert_eq!(snapshot1.messages[0].message, "queued A");

    fixture
        .client
        .send_message(&agent.stream, "queued B".to_owned())
        .await
        .expect("send_message B failed");

    let snapshot2 = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(snapshot2.messages.len(), 2);
    assert_eq!(snapshot2.messages[0].message, "queued A");
    assert_eq!(snapshot2.messages[1].message, "queued B");
}

#[tokio::test(start_paused = true)]
async fn fifo_drain_on_typing_status_false() {
    let mut fixture = Fixture::new().await;

    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-drain",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: drain-test",
                &gate,
            ))
            .then(MockTurn::text("mock backend response to: drain A"))
            .then(MockTurn::text("mock backend response to: drain B")),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "drain A".to_owned())
        .await
        .expect("send drain A");
    fixture
        .client
        .send_message(&agent.stream, "drain B".to_owned())
        .await
        .expect("send drain B");

    let before = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(before.messages[0].message, "drain A");
    assert_eq!(before.messages[1].message, "drain B");

    gate.release_one();

    let after = fixture.expect_queued_messages(&agent, 1).await;
    assert_eq!(
        after.messages.len(),
        1,
        "only B should remain after draining A"
    );
    assert_eq!(after.messages[0].message, "drain B");
}

#[tokio::test(start_paused = true)]
async fn duplicate_typing_false_drains_only_once() {
    let mut fixture = Fixture::new().await;

    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-duplicate-idle",
            MockScript::one(
                MockTurn::gated_text("duplicate-idle-test response", &gate).with_duplicate_idle(),
            )
            .then(MockTurn::text("duplicate A response"))
            .then(MockTurn::text("duplicate B response")),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    gate.wait_until_entered().await;

    fixture
        .client
        .send_message(&agent.stream, "duplicate A".to_owned())
        .await
        .expect("send duplicate A");
    fixture
        .client
        .send_message(&agent.stream, "duplicate B".to_owned())
        .await
        .expect("send duplicate B");

    let before = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(before.messages[0].message, "duplicate A");
    assert_eq!(before.messages[1].message, "duplicate B");
    gate.release_one();

    let after_first_idle = fixture.expect_queued_messages(&agent, 1).await;
    assert_eq!(after_first_idle.messages[0].message, "duplicate B");

    assert_queue_not_emptied_before_next_typing_true(
        &mut fixture.client,
        &agent.stream,
        "next queued turn should become busy before the queue drains again",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn cancel_queued_message_removes_entry() {
    let mut fixture = Fixture::new().await;

    // Gated launch turn: the agent stays busy for the whole test, so the
    // queued entry can only disappear through the cancel under test.
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-cancel",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: cancel-test",
                &gate,
            )),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "cancel me".to_owned())
        .await
        .expect("send cancel me");

    let snapshot = fixture.expect_queued_messages(&agent, 1).await;
    let cancel_id: QueuedMessageId = snapshot.messages[0].id.clone();

    fixture
        .client
        .cancel_queued_message(&agent.stream, CancelQueuedMessagePayload { id: cancel_id })
        .await
        .expect("cancel_queued_message failed");

    let empty = fixture.expect_queued_messages(&agent, 0).await;
    assert!(
        empty.messages.is_empty(),
        "queue must be empty after cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn edit_queued_message_preserves_identity_order_and_origin() {
    let mut fixture = Fixture::new().await;
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-edit",
            MockScript::one(MockTurn::gated_text("edit-test response", &gate)),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    let original_images = vec![ImageData {
        media_type: "image/png".to_owned(),
        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_owned(),
    }];
    fixture
        .client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: "edit A".to_owned(),
                images: Some(original_images.clone()),
                origin: Some(MessageOrigin::User),
                tool_response: None,
            },
        )
        .await
        .expect("send editable message");
    fixture
        .client
        .send_message(&agent.stream, "order B".to_owned())
        .await
        .expect("send second queued message");

    let before = fixture.expect_queued_messages(&agent, 2).await;
    let edited_id = before.messages[0].id.clone();
    let second_id = before.messages[1].id.clone();
    fixture
        .client
        .edit_queued_message(
            &agent.stream,
            EditQueuedMessagePayload {
                id: edited_id.clone(),
                message: "edited A\nwith detail".to_owned(),
                images: original_images.clone(),
            },
        )
        .await
        .expect("edit queued message");

    let after = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(after.messages[0].id, edited_id);
    assert_eq!(after.messages[1].id, second_id);
    assert_eq!(after.messages[0].message, "edited A\nwith detail");
    assert_eq!(after.messages[0].images, original_images);
    assert_eq!(after.messages[0].origin, Some(MessageOrigin::User));
}

#[tokio::test(start_paused = true)]
async fn edit_queued_message_rejects_supervisor_origin_without_mutation() {
    let mut fixture = Fixture::new().await;
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-edit-supervisor",
            MockScript::one(MockTurn::gated_text("edit-test response", &gate)),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: "supervisor-owned".to_owned(),
                images: None,
                origin: Some(MessageOrigin::Supervisor),
                tool_response: None,
            },
        )
        .await
        .expect("queue supervisor message");
    fixture
        .client
        .send_message(&agent.stream, "user-owned".to_owned())
        .await
        .expect("queue user message");

    let before = fixture.expect_queued_messages(&agent, 2).await;
    let supervisor_id = before.messages[0].id.clone();
    let user_id = before.messages[1].id.clone();
    fixture
        .client
        .edit_queued_message(
            &agent.stream,
            EditQueuedMessagePayload {
                id: supervisor_id.clone(),
                message: "client tried to overwrite supervisor".to_owned(),
                images: Vec::new(),
            },
        )
        .await
        .expect("submit forbidden edit");

    let env = fixture
        .next_frame_matching("supervisor edit rejection", |env| {
            env.kind == FrameKind::AgentError && env.stream == agent.stream
        })
        .await;
    let error: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(!error.fatal);
    assert!(error.message.contains("supervisor-origin"));

    fixture
        .client
        .send_message(&agent.stream, "snapshot trigger".to_owned())
        .await
        .expect("queue snapshot trigger");
    let after = fixture.expect_queued_messages(&agent, 3).await;
    assert_eq!(after.messages[0].id, supervisor_id);
    assert_eq!(after.messages[0].message, "supervisor-owned");
    assert_eq!(after.messages[0].origin, Some(MessageOrigin::Supervisor));
    assert_eq!(after.messages[1].id, user_id);
    assert_eq!(after.messages[1].message, "user-owned");
}

#[tokio::test(start_paused = true)]
async fn send_queued_message_now_reorders() {
    let mut fixture = Fixture::new().await;

    // Gated launch turn: the queue can only reorder, never drain, while the
    // turn is parked.
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-reorder",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: reorder-test",
                &gate,
            )),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "order A".to_owned())
        .await
        .expect("send order A");
    fixture
        .client
        .send_message(&agent.stream, "order B".to_owned())
        .await
        .expect("send order B");

    let snapshot_ab = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(snapshot_ab.messages[0].message, "order A");
    assert_eq!(snapshot_ab.messages[1].message, "order B");

    let b_id: QueuedMessageId = snapshot_ab.messages[1].id.clone();

    fixture
        .client
        .send_queued_message_now(
            &agent.stream,
            SendQueuedMessageNowPayload { id: b_id.clone() },
        )
        .await
        .expect("send_queued_message_now failed");

    let snapshot_ba = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(
        snapshot_ba.messages[0].id, b_id,
        "B must be first after SendQueuedMessageNow"
    );
    assert_eq!(snapshot_ba.messages[0].message, "order B");
    assert_eq!(snapshot_ba.messages[1].message, "order A");
}

#[tokio::test(start_paused = true)]
async fn queue_replays_to_new_subscriber() {
    let mut fixture = Fixture::new().await;

    // Gated launch turn: both messages are still queued when the second
    // subscriber connects, deterministically.
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-replay",
            MockScript::one(MockTurn::gated_text(
                "mock backend response to: replay-test",
                &gate,
            )),
        )
        .await;

    let agent_prefix = format!("/agent/{}/", agent.new_agent.agent_id.0);
    assert!(
        agent.stream.0.starts_with(&agent_prefix),
        "agent_id must be present in the instance stream path: {}",
        agent.stream.0
    );

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "replay A".to_owned())
        .await
        .expect("send replay A");
    fixture
        .client
        .send_message(&agent.stream, "replay B".to_owned())
        .await
        .expect("send replay B");

    let snapshot1 = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(snapshot1.messages.len(), 2);

    let mut client2 = fixture.connect().await;
    // The replay reaches a fresh subscriber on a NEW instance stream under the
    // same agent, so this must match by agent prefix, not client1's
    // `agent.stream`.
    let snapshot2 =
        fixture::expect_agent_queued_messages_on(&mut client2, &agent.new_agent.agent_id, 2).await;
    assert_eq!(
        snapshot2.messages.len(),
        2,
        "replayed queue must have 2 entries"
    );
    assert_eq!(
        snapshot2.messages[0].message, "replay A",
        "first replayed entry must be replay A"
    );
    assert_eq!(
        snapshot2.messages[1].message, "replay B",
        "second replayed entry must be replay B"
    );

    assert_eq!(snapshot2.messages[0].id, snapshot1.messages[0].id);
    assert_eq!(snapshot2.messages[1].id, snapshot1.messages[1].id);
}

#[tokio::test(start_paused = true)]
async fn queue_cleared_on_agent_termination() {
    let mut fixture = Fixture::new().await;

    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "queue-terminate",
            MockScript::one(MockTurn::busy_then_close_stream(&gate)),
        )
        .await;

    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    gate.wait_until_entered().await;

    fixture
        .client
        .send_message(&agent.stream, "will be lost A".to_owned())
        .await
        .expect("send lost A");
    fixture
        .client
        .send_message(&agent.stream, "will be lost B".to_owned())
        .await
        .expect("send lost B");

    let populated = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(populated.messages.len(), 2);
    gate.release_one();

    let cleared = fixture.expect_queued_messages(&agent, 0).await;
    assert!(
        cleared.messages.is_empty(),
        "queue must be empty after termination"
    );

    let env = fixture
        .next_frame_matching("fatal AgentError after termination", |env| {
            env.kind == FrameKind::AgentError && env.stream == agent.stream
        })
        .await;
    let err: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(err.fatal, "termination must produce a fatal AgentError");
}
