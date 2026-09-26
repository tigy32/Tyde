mod fixture;

use fixture::{Fixture, TestAgent};
use protocol::{
    AgentActivity, AgentActivityChangedPayload, AgentErrorPayload, BackendKind,
    CancelQueuedMessagePayload, ChatEvent, EditQueuedMessagePayload, FrameKind, ImageData,
    MessageOrigin, QueuedMessageId, QueuedMessagesPayload, SendMessagePayload,
    SendMessageToolResponse, SendQueuedMessageNowPayload, SpawnAgentParams, SpawnAgentPayload,
    StreamPath,
};
use server::backend::mock::{MockGateHandle, MockRequest, MockScript, MockTurn};

fn activity_edge(env: &protocol::Envelope, agent: &TestAgent) -> Option<AgentActivity> {
    (env.stream == agent.stream && env.kind == FrameKind::AgentActivityChanged).then(|| {
        env.parse_payload::<AgentActivityChangedPayload>()
            .expect("parse AgentActivityChanged")
            .activity
    })
}

/// Wait until `agent` has asked the user a question and stopped typing, and
/// the server has published that it is awaiting the user's answer.
async fn expect_live_question_awaiting_user(
    fixture: &mut Fixture,
    agent: &TestAgent,
) -> protocol::ToolRequest {
    let mut request = None;
    let mut paused = false;
    let mut activity = None;
    while !(request.is_some() && paused && activity == Some(AgentActivity::AwaitingUser)) {
        let env = fixture
            .next_frame_matching("question awaiting the user", |env| {
                env.stream == agent.stream
            })
            .await;
        if let Some(edge) = activity_edge(&env, agent) {
            activity = Some(edge);
            continue;
        }
        if env.kind != FrameKind::ChatEvent {
            continue;
        }
        match env.parse_payload::<ChatEvent>().expect("parse ChatEvent") {
            ChatEvent::ToolRequest(pending) => request = Some(pending),
            ChatEvent::TypingStatusChanged(false) => paused = true,
            ChatEvent::ToolExecutionCompleted(completion) => assert!(
                request
                    .as_ref()
                    .is_none_or(|pending| pending.tool_call_id != completion.tool_call_id),
                "question completed before it was answered or cancelled"
            ),
            _ => {}
        }
    }
    request.expect("question request")
}

/// Cancel `agent` while it awaits the user and assert the pending card is
/// retired as cancelled before the server settles the agent back to idle.
async fn cancel_question_awaiting_user(
    fixture: &mut Fixture,
    agent: &TestAgent,
    request: &protocol::ToolRequest,
) {
    fixture
        .client
        .interrupt(&agent.stream)
        .await
        .expect("cancel while awaiting the user");
    let mut cancelled = 0;
    loop {
        let env = fixture
            .next_frame_matching("cancelled question settles idle", |env| {
                env.stream == agent.stream
            })
            .await;
        if let Some(edge) = activity_edge(&env, agent) {
            assert_ne!(
                edge,
                AgentActivity::Thinking,
                "cancelling a pending question must not start work"
            );
            if edge == AgentActivity::Idle {
                break;
            }
            continue;
        }
        if env.kind == FrameKind::ChatEvent
            && let Ok(ChatEvent::ToolExecutionCompleted(completion)) =
                env.parse_payload::<ChatEvent>()
            && completion.tool_call_id == request.tool_call_id
        {
            assert!(
                matches!(
                    completion.outcome,
                    protocol::ToolExecutionOutcome::Cancelled { .. }
                ),
                "a cancelled question must be retired as cancelled, got {:?}",
                completion.outcome
            );
            cancelled += 1;
        }
    }
    assert_eq!(
        cancelled, 1,
        "cancel must retire the pending card exactly once before idle"
    );
}

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

    let gate = MockGateHandle::new();
    let cancel_blocking_gate = MockGateHandle::new();
    let cancel_async_gate = MockGateHandle::new();
    let question = fixture
        .spawn_scripted(
            "blocking-question",
            MockScript::one(MockTurn::blocking_question_request(
                "blocking-question",
                &gate,
            ))
            .then(MockTurn::text("answer accepted"))
            .then(MockTurn::text("queued question follow-up"))
            .then(MockTurn::blocking_question_request(
                "cancelled-blocking-question",
                &cancel_blocking_gate,
            ))
            .then(MockTurn::async_question_request(
                "cancelled-async-question",
                &cancel_async_gate,
            )),
        )
        .await;
    gate.wait_until_entered().await;
    gate.release_one();
    let request = expect_live_question_awaiting_user(&mut fixture, &question).await;
    assert!(matches!(
        request.tool_type,
        protocol::ToolRequestType::AskUserQuestion {
            mode: protocol::UserQuestionMode::Blocking,
            ..
        }
    ));
    fixture
        .client
        .send_message(&question.stream, "queued question follow-up".to_owned())
        .await
        .expect("send during blocking question");
    fixture.expect_queued_messages(&question, 1).await;
    // The turn stays open (the follow-up above queued behind the answer), but
    // the backend has stopped typing. A reconnecting client must see the same
    // awaiting-user state the live stream already reported: not thinking, and
    // not idle either, or it hides that the user owes an answer.
    let (mut mobile, bootstrap) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "blocking-question-phone",
    )
    .await;
    let descriptor = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == question.new_agent.agent_id)
        .expect("blocking question descriptor");
    assert_eq!(
        descriptor.activity,
        protocol::AgentActivity::AwaitingUser,
        "HostBootstrap must report a turn awaiting the user's answer as awaiting the user"
    );
    let question_stream = descriptor.instance_stream.clone();
    fixture::send_load_agent_on(&mut mobile, &question_stream).await;
    let agent_bootstrap: protocol::AgentBootstrapPayload =
        fixture::next_frame_matching_on(&mut mobile, "blocking question bootstrap", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == question_stream
        })
        .await
        .parse_payload()
        .expect("parse blocking question AgentBootstrap");
    assert_eq!(
        agent_bootstrap.activity,
        protocol::AgentActivity::AwaitingUser,
        "AgentBootstrap must report a turn awaiting the user's answer as awaiting the user"
    );
    assert!(agent_bootstrap.events.iter().any(|event| matches!(event,
        protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(pending))
            if pending.tool_call_id == request.tool_call_id)));
    drop(mobile);
    fixture
        .client
        .send_message_payload(
            &question.stream,
            SendMessagePayload {
                message: "BLUE".to_owned(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::AskUserQuestion {
                    tool_call_id: request.tool_call_id.clone(),
                    answer: "BLUE".to_owned(),
                }),
            },
        )
        .await
        .expect("answer blocking question");
    let answered = fixture.finish_turn(&question).await;
    assert_eq!(answered.chat_events().iter().filter(|event| matches!(event,
        ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == request.tool_call_id
            && matches!(completion.outcome, protocol::ToolExecutionOutcome::Succeeded { .. }))).count(), 1);
    let drained = fixture.finish_turn(&question).await;
    assert!(drained.chat_events().iter().any(|event| matches!(event,
        ChatEvent::StreamEnd(end) if end.message.content == "queued question follow-up")));
    assert!(answered.saw_queue_drained() || drained.saw_queue_drained());

    // Cancel withdraws a blocking question: the card is retired as cancelled
    // and the agent settles idle rather than waiting forever.
    fixture
        .client
        .send_message(&question.stream, "ask again".to_owned())
        .await
        .expect("send prompt that asks a blocking question");
    cancel_blocking_gate.wait_until_entered().await;
    cancel_blocking_gate.release_one();
    let blocking = expect_live_question_awaiting_user(&mut fixture, &question).await;
    cancel_question_awaiting_user(&mut fixture, &question, &blocking).await;

    // An async question left open after its turn ended has no running turn to
    // cancel, yet Cancel must still withdraw it.
    fixture
        .client
        .send_message(&question.stream, "ask asynchronously".to_owned())
        .await
        .expect("send prompt that asks an async question");
    cancel_async_gate.wait_until_entered().await;
    cancel_async_gate.release_one();
    let async_question = expect_live_question_awaiting_user(&mut fixture, &question).await;
    assert!(matches!(
        async_question.tool_type,
        protocol::ToolRequestType::AskUserQuestion {
            mode: protocol::UserQuestionMode::NonBlocking,
            ..
        }
    ));
    cancel_question_awaiting_user(&mut fixture, &question, &async_question).await;

    let (mut late, bootstrap) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "cancelled-question-phone",
    )
    .await;
    let descriptor = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == question.new_agent.agent_id)
        .expect("cancelled question descriptor");
    assert_eq!(
        descriptor.activity,
        protocol::AgentActivity::Idle,
        "a withdrawn question no longer awaits the user"
    );
    let question_stream = descriptor.instance_stream.clone();
    fixture::send_load_agent_on(&mut late, &question_stream).await;
    let late_bootstrap: protocol::AgentBootstrapPayload =
        fixture::next_frame_matching_on(&mut late, "cancelled question bootstrap", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == question_stream
        })
        .await
        .parse_payload()
        .expect("parse cancelled question AgentBootstrap");
    assert_eq!(late_bootstrap.activity, protocol::AgentActivity::Idle);
    for cancelled in [&blocking, &async_question] {
        assert_eq!(
            late_bootstrap
                .events
                .iter()
                .filter(|event| matches!(event,
                    protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
                        if completion.tool_call_id == cancelled.tool_call_id
                            && matches!(completion.outcome, protocol::ToolExecutionOutcome::Cancelled { .. })))
                .count(),
            1,
            "history must record each withdrawn question as cancelled exactly once"
        );
    }
    fixture.mock(&question).await.assert_clean().await;
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

    for malformed in [
        serde_json::json!({"kind": "AskUserQuestion", "tool_call_id": "missing-answer"}),
        serde_json::json!({"kind": "AskUserQuestion", "tool_call_id": "wrong-answer", "answer": []}),
        serde_json::json!({"kind": "AskUserQuestion", "tool_call_id": null, "answer": "é\nanswer"}),
    ] {
        let seq = fixture
            .client
            .outgoing_seq
            .get_mut(&agent.stream)
            .expect("agent sequence");
        let envelope = protocol::Envelope {
            stream: agent.stream.clone(),
            seq: *seq,
            kind: FrameKind::SendMessage,
            payload: serde_json::json!({"message": "answer", "tool_response": malformed}),
        };
        *seq += 1;
        protocol::write_envelope(&mut fixture.client.writer, &envelope)
            .await
            .expect("send malformed answer");
        let error = fixture::next_frame_matching_on(
            &mut fixture.client,
            "malformed answer rejection",
            |env| env.kind == FrameKind::CommandError,
        )
        .await;
        let error: protocol::CommandErrorPayload = error.parse_payload().expect("command error");
        assert!(
            !error.fatal,
            "malformed answer must not close the connection"
        );
        assert_eq!(error.request_kind, FrameKind::SendMessage);
        assert_eq!(error.stream, agent.stream);
        assert_eq!(error.code, protocol::CommandErrorCode::InvalidInput);
    }

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

fn steer_payload(message: &str) -> SendMessagePayload {
    SendMessagePayload {
        message: message.to_owned(),
        images: None,
        origin: None,
        tool_response: None,
    }
}

fn is_user_message(event: &ChatEvent, content: &str) -> bool {
    matches!(
        event,
        ChatEvent::MessageAdded(message)
            if matches!(message.sender, protocol::MessageSender::User) && message.content == content
    )
}

/// Explicit steering joins the running turn, but sending a queued message
/// now cancels it and starts a new turn, even when steering is supported.
/// Once idle, a steer remains an ordinary send.
#[tokio::test(start_paused = true)]
async fn steer_and_send_now_use_distinct_delivery_modes() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "queue-steer",
            MockScript::one(MockTurn::held_text("launch reply"))
                .then(MockTurn::text("send-now reply"))
                .then(MockTurn::text("idle steer reply"))
                .with_user_bubbles()
                .with_mid_turn_steering(),
        )
        .await;
    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "queued behind the turn".to_owned())
        .await
        .expect("send queued message");
    let queued = fixture.expect_queued_messages(&agent, 1).await;

    fixture
        .client
        .steer_message_payload(&agent.stream, steer_payload("steer into the turn"))
        .await
        .expect("steer_message failed");
    fixture
        .next_chat_event_matching(&agent, "steered user message", |event| {
            is_user_message(event, "steer into the turn")
        })
        .await;

    fixture
        .client
        .send_queued_message_now(
            &agent.stream,
            SendQueuedMessageNowPayload {
                id: queued.messages[0].id.clone(),
            },
        )
        .await
        .expect("send_queued_message_now failed");
    fixture.expect_queued_messages(&agent, 0).await;
    fixture
        .next_chat_event_matching(&agent, "send-now user message", |event| {
            is_user_message(event, "queued behind the turn")
        })
        .await;

    let mock = fixture.mock(&agent).await;
    let requests = mock.requests().await;
    assert!(
        matches!(
            requests.as_slice(),
            [
                MockRequest::Launch { .. },
                MockRequest::Steer(first),
                MockRequest::Interrupt,
                MockRequest::Input(second),
            ] if first.message == "steer into the turn" && second.message == "queued behind the turn"
        ),
        "send-now must interrupt and send, while explicit steering must not interrupt"
    );

    fixture
        .next_chat_event_matching(&agent, "send-now turn idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;

    fixture
        .client
        .steer_message_payload(&agent.stream, steer_payload("steer while idle"))
        .await
        .expect("idle steer_message failed");
    fixture
        .next_chat_event_matching(&agent, "idle steer turn idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let requests = mock.requests().await;
    assert!(
        matches!(
            requests.last(),
            Some(MockRequest::Input(payload)) if payload.message == "steer while idle"
        ),
        "an idle steer must be delivered as an ordinary send"
    );
    mock.assert_clean().await;
}

/// A backend that cannot take input mid-turn still honours a steer: the server
/// queues the message first, interrupts the turn, and sends it once idle.
#[tokio::test(start_paused = true)]
async fn steer_message_interrupts_when_backend_cannot_steer() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "queue-steer-fallback",
            MockScript::one(MockTurn::held_text("holding"))
                .then(MockTurn::text("redirected reply"))
                .then(MockTurn::text("queued reply"))
                .with_user_bubbles(),
        )
        .await;
    fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "queued earlier".to_owned())
        .await
        .expect("send queued message");
    fixture.expect_queued_messages(&agent, 1).await;

    fixture
        .client
        .steer_message_payload(&agent.stream, steer_payload("redirect now"))
        .await
        .expect("steer_message failed");
    let snapshot = fixture.expect_queued_messages(&agent, 2).await;
    assert_eq!(
        snapshot.messages[0].message, "redirect now",
        "a steer that falls back must jump the queue"
    );

    fixture
        .next_chat_event_matching(&agent, "OperationCancelled", |event| {
            matches!(event, ChatEvent::OperationCancelled(_))
        })
        .await;
    fixture
        .next_chat_event_matching(&agent, "redirected user message", |event| {
            is_user_message(event, "redirect now")
        })
        .await;

    let requests = fixture.mock(&agent).await.requests().await;
    assert!(
        matches!(
            requests.as_slice(),
            [
                MockRequest::Launch { .. },
                MockRequest::Interrupt,
                MockRequest::Input(payload),
                ..
            ] if payload.message == "redirect now"
        ),
        "the fallback must interrupt, then send the steered message first: {requests:?}"
    );
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

async fn spawn_restart_agent(
    fixture: &mut Fixture,
    name: &str,
    parent_agent_id: Option<protocol::AgentId>,
    script: Option<MockScript>,
) -> (fixture::TestAgent, protocol::SessionId) {
    let reservation = match script {
        Some(script) => Some(fixture.reserve_next_mock_launch(name, script).await),
        None => None,
    };
    let (agent, start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![format!("/tmp/{}", name.replace(' ', "-"))],
                prompt: format!("{name} prompt"),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    drop(reservation);
    (agent, start.session_id.expect("spawned agent session id"))
}

/// The status a context-compaction frame reports for `agent`, if it is one.
fn compaction_status_on(env: &protocol::Envelope, agent: &TestAgent) -> Option<String> {
    (env.stream == agent.stream && env.kind == FrameKind::ContextCompactionNotify).then(|| {
        let payload: protocol::ContextCompactionNotifyPayload =
            env.parse_payload().expect("parse ContextCompactionNotify");
        format!("{:?}", payload.status)
    })
}

/// A steer and a send-now the router handed to an agent before a restart
/// began, but that its actor only reaches after the restart has prepared it,
/// must neither redirect the provider's turn nor be lost: both stay durably
/// queued for the host that comes back.
#[tokio::test]
async fn restart_stop_holds_redirects_mailboxed_before_the_stop() {
    let mut fixture = Fixture::new().await;
    let launch_gate = MockGateHandle::new();
    let send_gate = MockGateHandle::new();
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "restart redirect",
        None,
        Some(
            MockScript::one(MockTurn::gated_text("launch reply", &launch_gate))
                .then(MockTurn::held_text("drained turn"))
                .with_send_gate(&send_gate)
                .with_mid_turn_steering(),
        ),
    )
    .await;
    let mock = fixture.mock(&agent).await;
    launch_gate.wait_until_entered().await;
    fixture
        .client
        .send_message(&agent.stream, "queued first".to_owned())
        .await
        .expect("queue first");
    fixture.expect_queued_messages(&agent, 1).await;
    fixture
        .client
        .send_message(&agent.stream, "queued second".to_owned())
        .await
        .expect("queue second");
    let queued = fixture.expect_queued_messages(&agent, 2).await;
    let second_id = queued.messages[1].id.clone();
    let (witness, _) = spawn_restart_agent(
        &mut fixture,
        "restart redirect witness",
        None,
        Some(MockScript::one(MockTurn::held_text("witness held"))),
    )
    .await;

    // The launch turn ends and the drain hands "queued first" to the provider,
    // which holds the actor inside that send.
    launch_gate.release_one();
    send_gate.wait_until_entered().await;
    fixture
        .client
        .steer_message_payload(&agent.stream, steer_payload("steer during stop"))
        .await
        .expect("steer the held agent");
    fixture
        .client
        .send_queued_message_now(
            &agent.stream,
            SendQueuedMessageNowPayload {
                id: second_id.clone(),
            },
        )
        .await
        .expect("send the second message now");
    // The connection routes frames in order, so once the witness has queued
    // this message both redirects are in the held agent's mailbox.
    fixture
        .client
        .send_message(&witness.stream, "witness queued".to_owned())
        .await
        .expect("queue on the witness");
    fixture.expect_queued_messages(&witness, 1).await;

    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let stop_host = host.clone();
    let stop = tokio::spawn(async move {
        stop_host.shutdown_for_restart().await;
    });
    stop_gate.wait_until_entered().await;
    send_gate.release_one();
    stop_gate.release_one();
    tokio::time::timeout(std::time::Duration::from_secs(27), stop)
        .await
        .expect("restart stop completes")
        .expect("restart stop task");

    // The restart stop itself interrupts the provider once; nothing else
    // may redirect it.
    let requests = mock.requests().await;
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, MockRequest::Steer(_))),
        "a steer reached after the restart began must not reach the provider: {requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request, MockRequest::Interrupt))
            .count(),
        1,
        "a send-now reached after the restart began must not cancel the turn: {requests:?}"
    );
    let inputs = requests
        .iter()
        .filter_map(|request| match request {
            MockRequest::Input(payload) => Some(payload.message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(inputs, vec!["queued first"]);
    let store = server::store::session::SessionStore::load(fixture.session_store_path())
        .expect("read durable store");
    let record = store
        .list()
        .expect("list durable records")
        .into_iter()
        .find(|record| record.id == session)
        .expect("durable record for the redirected agent");
    let record = serde_json::to_value(&record).expect("inspect durable record");
    let mut kept = record["queued_messages"]
        .as_array()
        .expect("queued messages array")
        .iter()
        .map(|entry| entry["message"].as_str().expect("queued text").to_owned())
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(
        kept,
        vec!["queued second".to_owned(), "steer during stop".to_owned()],
        "redirects refused by the restart stay durably queued"
    );
    assert_eq!(
        record["turn_recovery"],
        serde_json::json!("InterruptedByRestart")
    );
}

/// Once a restart has begun no agent may start work: not a held turn's queue,
/// not a message sent to an idle agent (it is refused), not a compaction
/// deferred behind a turn the restart ends, and not
/// the continuation of a restored turn whose replay finishes while a second
/// restart is already under way.
#[tokio::test]
async fn restart_stop_preserves_durable_turn_and_queue() {
    let mut fixture = Fixture::new().await;
    let shutdown_gate = server::backend::mock::MockGateHandle::new();
    let durable_replay = server::backend::mock::MockResumeReplay::default();
    let (agent, durable_session) = spawn_restart_agent(
        &mut fixture,
        "restart durable",
        None,
        Some(
            MockScript::one(MockTurn::held_text("held response"))
                .with_shutdown_gate(&shutdown_gate)
                .with_controlled_resume_replay(&durable_replay),
        ),
    )
    .await;
    fixture
        .client
        .send_message(&agent.stream, "queued first".to_owned())
        .await
        .expect("queue first");
    fixture.expect_queued_messages(&agent, 1).await;
    fixture
        .client
        .send_message(&agent.stream, "queued second".to_owned())
        .await
        .expect("queue second");
    fixture.expect_queued_messages(&agent, 2).await;

    // An orchestrator parked in tyde_await_agents on a live child. The restart
    // shutdown expires that await, so the orchestrator's turn can end while
    // the shutdown is still under way; nothing it has queued may start then.
    let (parent, parent_session) =
        spawn_restart_agent(&mut fixture, "restart orchestrator", None, None).await;
    fixture.finish_turn(&parent).await;
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "restart worker",
        Some(parent.new_agent.agent_id.clone()),
        Some(MockScript::one(MockTurn::held_text("child unfinished"))),
    )
    .await;
    fixture
        .next_chat_event_matching(&child, "child held turn streamed", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    fixture
        .mock(&parent)
        .await
        .enqueue(MockTurn::agent_control_await(vec![
            child.new_agent.agent_id.clone(),
        ]))
        .await;
    fixture
        .client
        .send_message(&parent.stream, "await the worker".to_owned())
        .await
        .expect("send parent await prompt");
    fixture
        .next_chat_event_matching(&parent, "parent parked in await", |event| {
            matches!(event, ChatEvent::ToolRequest(request) if fixture::tool_request_name(request) == "tyde_await_agents")
        })
        .await;
    fixture
        .client
        .send_message(&parent.stream, "parent queued".to_owned())
        .await
        .expect("queue on parent");
    fixture.expect_queued_messages(&parent, 1).await;
    let (bystander, bystander_session) =
        spawn_restart_agent(&mut fixture, "restart bystander", None, None).await;
    fixture.finish_turn(&bystander).await;

    // A second orchestrator, parked the same way, with a compaction deferred
    // behind its await. The restart ends that turn, which is exactly when the
    // deferred compaction would otherwise start.
    let (compactor, _) = spawn_restart_agent(&mut fixture, "restart compactor", None, None).await;
    fixture.finish_turn(&compactor).await;
    let (compactor_child, _) = spawn_restart_agent(
        &mut fixture,
        "restart compactor worker",
        Some(compactor.new_agent.agent_id.clone()),
        Some(MockScript::one(MockTurn::held_text(
            "compactor child unfinished",
        ))),
    )
    .await;
    fixture
        .next_chat_event_matching(
            &compactor_child,
            "compactor child held turn streamed",
            |event| matches!(event, ChatEvent::StreamEnd(_)),
        )
        .await;
    fixture
        .mock(&compactor)
        .await
        .enqueue(MockTurn::agent_control_await(vec![
            compactor_child.new_agent.agent_id.clone(),
        ]))
        .await;
    fixture
        .client
        .send_message(&compactor.stream, "await the compactor worker".to_owned())
        .await
        .expect("send compactor await prompt");
    fixture
        .next_chat_event_matching(&compactor, "compactor parked in await", |event| {
            matches!(event, ChatEvent::ToolRequest(request) if fixture::tool_request_name(request) == "tyde_await_agents")
        })
        .await;
    fixture
        .client
        .compact_agent(&compactor.stream, protocol::AgentCompactPayload::default())
        .await
        .expect("request compaction behind the await");
    let deferred = fixture
        .next_frame_matching("compaction deferred behind the await", |env| {
            compaction_status_on(env, &compactor).is_some()
        })
        .await;
    let deferred = compaction_status_on(&deferred, &compactor).expect("compaction status");
    assert!(
        deferred.starts_with("Deferred"),
        "a compaction requested mid-turn waits for idle: {deferred}"
    );

    let store = server::store::session::SessionStore::load(fixture.session_store_path())
        .expect("read durable store");
    let before = store.list().expect("list durable records");
    assert_eq!(before.len(), 6);
    let record_for = |records: &[server::store::session::SessionRecord],
                      session: &protocol::SessionId|
     -> serde_json::Value {
        let record = records
            .iter()
            .find(|record| &record.id == session)
            .unwrap_or_else(|| panic!("no durable record for the spawned session"));
        serde_json::to_value(record).expect("inspect durable record")
    };
    for session in [&durable_session, &parent_session, &child_session] {
        assert_eq!(
            record_for(&before, session)["turn_recovery"],
            serde_json::json!("InFlight"),
            "held live turns must be durably marked before restart"
        );
    }

    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let first_host = host.clone();
    let first = tokio::spawn(async move {
        first_host.shutdown_for_restart().await;
    });
    stop_gate.wait_until_entered().await;
    // Every agent is prepared for the restart and both orchestrators' awaits
    // have been expired, ending their turns, but no agent has received its
    // stop command yet.
    let mut turn_ended = [false, false];
    while turn_ended != [true, true] {
        let env = fixture
            .next_frame_matching(
                "orchestrator turns end once the restart expires their awaits",
                |env| {
                    (env.stream == parent.stream || env.stream == compactor.stream)
                        && matches!(
                            env.kind,
                            FrameKind::ChatEvent | FrameKind::ContextCompactionNotify
                        )
                },
            )
            .await;
        if let Some(status) = compaction_status_on(&env, &compactor) {
            assert!(
                status.starts_with("Deferred"),
                "a compaction must not start once a restart has begun: {status}"
            );
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::TypingStatusChanged(false)) {
            turn_ended[usize::from(env.stream == compactor.stream)] = true;
        }
    }
    let bystander_requests = fixture.mock(&bystander).await.requests().await.len();
    fixture
        .client
        .send_message(&bystander.stream, "sent during restart".to_owned())
        .await
        .expect("message an idle agent during the restart");
    fixture
        .next_frame_matching("message refused during the restart", |env| {
            env.stream == bystander.stream && env.kind == FrameKind::AgentError
        })
        .await;
    // The mailbox round trips order these reads after the refused message and
    // the ended turn were handled, so anything they dispatched is visible.
    let bystander_after = fixture.mock(&bystander).await.requests().await;
    assert_eq!(
        bystander_after.len(),
        bystander_requests,
        "a message sent during a restart must not reach the backend: {bystander_after:?}"
    );
    fixture.mock(&compactor).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(750);
    while let Ok(Ok(Some(env))) =
        tokio::time::timeout_at(deadline, fixture.client.next_event()).await
    {
        if let Some(status) = compaction_status_on(&env, &compactor) {
            assert!(
                status.starts_with("Deferred"),
                "a compaction must not start once a restart has begun: {status}"
            );
        }
    }
    stop_gate.release_one();
    shutdown_gate.wait_until_entered().await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("first waiter cancelled")
            .is_cancelled()
    );
    tokio::time::timeout(std::time::Duration::from_secs(27), async {
        tokio::join!(host.shutdown_for_restart(), host.shutdown_for_restart());
    })
    .await
    .expect("all callers must share the bounded shutdown despite a stuck backend");
    let after = store.list().expect("list stopped records");
    let queued_messages = |record: &serde_json::Value| -> Vec<(String, String)> {
        record["queued_messages"]
            .as_array()
            .expect("queued messages array")
            .iter()
            .map(|entry| {
                (
                    entry["id"].as_str().expect("queued id").to_owned(),
                    entry["message"].as_str().expect("queued text").to_owned(),
                )
            })
            .collect()
    };
    let durable_after = record_for(&after, &durable_session);
    assert_eq!(
        queued_messages(&durable_after),
        queued_messages(&record_for(&before, &durable_session)),
        "a held turn's queue survives the restart stop unchanged"
    );
    assert_eq!(queued_messages(&durable_after).len(), 2);
    assert_eq!(
        durable_after["turn_recovery"],
        serde_json::json!("InterruptedByRestart")
    );
    assert!(
        !durable_after["restore_state"].is_null(),
        "restart must preserve restore intent"
    );
    let parent_after = record_for(&after, &parent_session);
    assert_eq!(
        queued_messages(&parent_after)
            .iter()
            .map(|(_, message)| message.as_str())
            .collect::<Vec<_>>(),
        vec!["parent queued"],
        "a turn that ends because the restart expired its await must not start queued work"
    );
    assert_eq!(
        parent_after["turn_recovery"],
        serde_json::json!("InterruptedByRestart"),
        "an await cut short by the restart is an interruption, not a completed turn"
    );
    assert_eq!(
        record_for(&after, &child_session)["turn_recovery"],
        serde_json::json!("InterruptedByRestart")
    );
    let bystander_after = record_for(&after, &bystander_session);
    assert!(
        queued_messages(&bystander_after).is_empty(),
        "a refused message is not silently kept"
    );
    assert!(bystander_after["turn_recovery"].is_null());

    // A restored turn whose replay finishes after the next restart has begun
    // must not be continued, nor start its queue: it stays interrupted for
    // the host that comes back.
    let bootstrap = fixture.restart_host().await;
    durable_replay.wait_until_started().await;
    let restored_durable = match bootstrap
        .agents
        .iter()
        .find(|agent| agent.session_id.as_ref() == Some(&durable_session))
    {
        Some(agent) => agent.clone(),
        None => fixture
            .next_frame_matching("restored durable agent", |env| {
                env.kind == FrameKind::NewAgent
                    && env
                        .parse_payload::<protocol::NewAgentPayload>()
                        .is_ok_and(|agent| agent.session_id.as_ref() == Some(&durable_session))
            })
            .await
            .parse_payload()
            .expect("parse restored NewAgent"),
    };
    let second_stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let second_host = fixture.host_for_test();
    let second = tokio::spawn(async move {
        second_host.shutdown_for_restart().await;
    });
    second_stop_gate.wait_until_entered().await;
    durable_replay.complete();
    let restored_bootstrap: protocol::AgentBootstrapPayload = fixture
        .next_frame_matching("restored durable bootstrap", |env| {
            env.stream == restored_durable.instance_stream && env.kind == FrameKind::AgentBootstrap
        })
        .await
        .parse_payload()
        .expect("parse restored bootstrap");
    let phases = restored_bootstrap
        .events
        .iter()
        .filter_map(|event| match event {
            protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::RestartRecovery { phase }) => {
                Some(phase.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        phases,
        vec![protocol::RestartRecoveryPhase::Interrupted {
            cause: protocol::RestartInterruptionCause::HostRestart
        }],
        "a restart under way must not continue a replayed turn"
    );
    let restored_requests = fixture
        .mock_by_id(&restored_durable.agent_id)
        .await
        .requests()
        .await;
    assert!(
        !restored_requests
            .iter()
            .any(|request| matches!(request, MockRequest::Input(_))),
        "neither the continuation nor the queue may reach the backend: {restored_requests:?}"
    );
    second_stop_gate.release_one();
    tokio::time::timeout(std::time::Duration::from_secs(27), second)
        .await
        .expect("second restart stop is bounded")
        .expect("second restart stop");
    let stopped_again = store.list().expect("list records after the second stop");
    let durable_again = record_for(&stopped_again, &durable_session);
    assert_eq!(
        durable_again["turn_recovery"],
        serde_json::json!("InterruptedByRestart"),
        "the uncontinued turn stays interrupted"
    );
    assert_eq!(
        queued_messages(&durable_again),
        queued_messages(&durable_after),
        "the uncontinued turn's queue is untouched"
    );
}
