mod fixture;

use fixture::{Fixture, next_logical_frame_matching_on};
use protocol::{
    AgentErrorPayload, BackendKind, ChatEvent, ExitPlanModeDecision, FrameKind,
    ListSessionsPayload, MessageSender, SendMessageToolResponse, SessionListPayload,
    SpawnAgentParams, SpawnAgentPayload,
};
use server::backend::mock::{MockGateHandle, MockRequest, MockScript, MockTurn, MockViolation};

#[tokio::test]
async fn reserved_script_governs_launch_turn() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "scripted-worker",
            MockScript::one(MockTurn::text("scripted launch response")),
        )
        .await;

    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("scripted launch response");
    for event in turn.chat_events() {
        if let ChatEvent::StreamEnd(end) = event {
            assert!(
                !end.message.content.contains("mock backend response to:"),
                "launch turn fell back to the default echo: {:?}",
                end.message.content
            );
        }
    }

    fixture.mock(&agent).await.assert_clean().await;
}

#[tokio::test]
async fn reservation_name_mismatch_fails_spawn_visibly() {
    let mut fixture = Fixture::new().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "the-reserved-name",
            MockScript::one(MockTurn::text("reserved response")),
        )
        .await;

    let (mismatched, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("some-other-name".to_owned()),
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
        .await;
    // The failure frames arrive inside the agent's bootstrap, which
    // `spawn_with` already unpacked into the logical pending queue.
    let env = next_logical_frame_matching_on(&mut fixture.client, "mismatch AgentError", |env| {
        env.kind == FrameKind::AgentError && env.stream == mismatched.stream
    })
    .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(
        payload
            .message
            .contains("mock launch reservation expected the next mock spawn to be named"),
        "unexpected mismatch error message: {}",
        payload.message
    );
    assert!(payload.fatal, "reservation mismatch must be fatal");

    // The mismatching spawn did not consume the reservation: the spawn it was
    // reserved for still gets the script.
    let reserved = fixture.spawn("the-reserved-name", "any prompt").await;
    let turn = fixture.finish_turn(&reserved).await;
    turn.assert_stream_end_contains("reserved response");
    drop(reservation);
}

#[tokio::test]
async fn gate_parks_scripted_turn_until_released() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted("gated", MockScript::one(MockTurn::text("launch response")))
        .await;
    fixture.finish_turn(&agent).await;

    let gate = MockGateHandle::new();
    let mock = fixture.mock(&agent).await;
    mock.enqueue(MockTurn::gated_text("gated response", &gate))
        .await;

    fixture
        .client
        .send_message(&agent.stream, "go".to_owned())
        .await
        .expect("send gated-turn message");
    gate.wait_until_entered().await;

    // Everything scripted before the gate has already streamed by the time
    // the gate reports entry.
    fixture
        .next_chat_event_matching(&agent, "gated turn stream end", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end) if end.message.content.contains("gated response")
            )
        })
        .await;

    // The control surface stays servicable while the turn is parked: reads
    // are answered mid-park, `assert_clean` reports the unreleased gate, and
    // a script can be extended without waiting for the release.
    let parked_requests = mock.requests().await;
    assert!(
        parked_requests.iter().any(
            |request| matches!(request, MockRequest::Input(payload) if payload.message == "go")
        ),
        "requests must be readable while parked: {parked_requests:?}"
    );
    let parked = mock.clone();
    let result = tokio::spawn(async move { parked.assert_clean().await }).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "assert_clean must report the unreleased gate while parked"
    );
    mock.enqueue(MockTurn::text("queued response")).await;

    // Provably mid-turn: a message sent now must queue instead of starting a
    // turn.
    fixture
        .client
        .send_message(&agent.stream, "queued while gated".to_owned())
        .await
        .expect("send message while gated");
    fixture.expect_queued_messages(&agent, 1).await;

    gate.release_one();
    // The gated turn's trailing idle, the queue drain, and the queued
    // message's own scripted turn are all causally behind the release; seeing
    // the queued message's response proves the release completed the turn.
    fixture
        .next_chat_event_matching(&agent, "queued message response", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end) if end.message.content.contains("queued response")
            )
        })
        .await;

    mock.assert_clean().await;
}

#[tokio::test]
async fn dropping_gate_handle_releases_the_turn() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "drop-gate",
            MockScript::one(MockTurn::text("launch response")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    let gate = MockGateHandle::new();
    let mock = fixture.mock(&agent).await;
    mock.enqueue(MockTurn::gated_text("gated response", &gate))
        .await;
    fixture
        .client
        .send_message(&agent.stream, "go".to_owned())
        .await
        .expect("send gated-turn message");
    gate.wait_until_entered().await;

    drop(gate);
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("gated response");
    mock.assert_clean().await;
}

#[tokio::test]
async fn requests_capture_launch_inputs_and_interrupts() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "captured",
            MockScript::one(MockTurn::text("launch response"))
                .then(MockTurn::held_text("holding open")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "hold now".to_owned())
        .await
        .expect("send held-turn message");
    fixture
        .next_chat_event_matching(&agent, "held turn stream end", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end) if end.message.content.contains("holding open")
            )
        })
        .await;

    fixture
        .client
        .interrupt(&agent.stream)
        .await
        .expect("interrupt held turn");
    fixture
        .next_chat_event_matching(&agent, "held turn cancelled", |event| {
            matches!(event, ChatEvent::OperationCancelled(_))
        })
        .await;

    let requests = fixture.mock(&agent).await.requests().await;
    assert_eq!(
        requests.len(),
        3,
        "expected launch + input + interrupt, got {requests:?}"
    );
    assert!(
        matches!(&requests[0], MockRequest::Launch { message } if message == "scripted launch"),
        "first capture should be the launch prompt: {requests:?}"
    );
    assert!(
        matches!(&requests[1], MockRequest::Input(payload) if payload.message == "hold now"),
        "second capture should be the delivered input: {requests:?}"
    );
    assert!(
        matches!(&requests[2], MockRequest::Interrupt),
        "third capture should be the interrupt: {requests:?}"
    );
}

#[tokio::test]
async fn explicit_script_exhaustion_is_loud_and_closes_the_backend() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "exhausted",
            MockScript::one(MockTurn::text("launch response")),
        )
        .await;
    // Taken before the close: the control must remain readable afterwards.
    let mock = fixture.mock(&agent).await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "unexpected extra input".to_owned())
        .await
        .expect("send unscripted message");

    let card = fixture
        .next_chat_event_matching(&agent, "exhaustion error card", |event| {
            matches!(
                event,
                ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error)
            )
        })
        .await;
    let ChatEvent::MessageAdded(message) = card else {
        unreachable!("matched MessageAdded above");
    };
    assert!(
        message.content.contains("mock backend script exhausted")
            && message.content.contains("unexpected extra input"),
        "exhaustion card must name the failure and the input: {:?}",
        message.content
    );

    let env = fixture
        .next_frame_matching("fatal AgentError after exhaustion", |env| {
            env.kind == FrameKind::AgentError
                && env.stream == agent.stream
                && env
                    .parse_payload::<AgentErrorPayload>()
                    .is_ok_and(|payload| payload.fatal)
        })
        .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(payload.fatal, "exhaustion must close the mock backend");

    // The strictness surface survives the close it reports on: the actor
    // published its terminal report before the control mailbox closed.
    let violations = mock.violations().await;
    assert!(
        matches!(
            &violations[..],
            [MockViolation::ScriptExhausted { message }] if message == "unexpected extra input"
        ),
        "terminal violations must record the exhaustion: {violations:?}"
    );
    let closed = mock.clone();
    let result = tokio::spawn(async move { closed.assert_clean().await }).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "assert_clean must fail on the terminal violation report"
    );
}

#[tokio::test]
async fn assert_clean_flags_unconsumed_turns() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "cleanly",
            MockScript::one(MockTurn::text("launch response")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    let mock = fixture.mock(&agent).await;
    mock.enqueue(MockTurn::text("second response")).await;

    let unclean = mock.clone();
    let result = tokio::spawn(async move { unclean.assert_clean().await }).await;
    assert!(
        result.is_err_and(|error| error.is_panic()),
        "assert_clean must panic while a scripted turn is unconsumed"
    );

    fixture
        .client
        .send_message(&agent.stream, "consume it".to_owned())
        .await
        .expect("send message consuming the script");
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("second response");
    mock.assert_clean().await;
}

#[tokio::test]
async fn explicit_prompts_survive_resume_replay() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "recorded",
            MockScript::one(MockTurn::text("first response"))
                .then(MockTurn::text("second response")),
        )
        .await;
    fixture.finish_turn(&agent).await;
    fixture
        .client
        .send_message(&agent.stream, "alpha".to_owned())
        .await
        .expect("send second scripted message");
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions");
    let env = fixture
        .next_frame_matching("SessionList", |env| env.kind == FrameKind::SessionList)
        .await;
    let list: SessionListPayload = env.parse_payload().expect("parse SessionListPayload");
    assert_eq!(
        list.sessions.len(),
        1,
        "fixture store should hold one session"
    );
    let session_id = list.sessions[0].id.clone();

    let reservation = fixture
        .reserve_next_mock_launch("recorded-resume", MockScript::one(MockTurn::history_join()))
        .await;
    let (resumed, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("recorded-resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some("show recorded history".to_owned()),
            },
        })
        .await;
    drop(reservation);

    fixture
        .next_chat_event_matching(&resumed, "recorded prompt history", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end)
                    if end.message.content.contains("mock history: scripted launch | alpha")
            )
        })
        .await;
}

#[tokio::test]
async fn explicit_tool_response_consumes_next_scripted_turn() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "planner",
            MockScript::one(MockTurn::text("launch response"))
                .then(MockTurn::exit_plan_request(
                    "epm-scripted",
                    "# Scripted plan",
                ))
                .then(MockTurn::text("post-approval response")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "make a plan".to_owned())
        .await
        .expect("send plan request message");
    let request = fixture
        .expect_paused_tool_request(&agent, "ExitPlanMode")
        .await;
    assert_eq!(request.tool_call_id, "epm-scripted");

    fixture.approve_exit_plan_mode(&agent, &request).await;
    fixture
        .next_chat_event_matching(&agent, "ExitPlanMode completion", |event| {
            matches!(
                event,
                ChatEvent::ToolExecutionCompleted(done)
                    if done.tool_call_id == "epm-scripted"
                        && fixture::tool_completion_succeeded(done)
            )
        })
        .await;
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("post-approval response");
    for event in turn.chat_events() {
        if let ChatEvent::StreamEnd(end) = event {
            assert!(
                !end.message.content.contains("mock ExitPlanMode approved"),
                "explicit continuation must not run the default echo follow-up: {:?}",
                end.message.content
            );
        }
    }

    let mock = fixture.mock(&agent).await;
    let requests = mock.requests().await;
    assert!(
        requests.iter().any(|request| matches!(
            request,
            MockRequest::ToolResponse(SendMessageToolResponse::ExitPlanMode {
                tool_call_id,
                decision,
                ..
            }) if tool_call_id == "epm-scripted" && *decision == ExitPlanModeDecision::Approve
        )),
        "the approval must be captured as a typed tool response: {requests:?}"
    );
    mock.assert_clean().await;
}

#[tokio::test]
async fn explicit_tool_response_without_continuation_is_loud() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "planner-exhausted",
            MockScript::one(MockTurn::text("launch response")).then(MockTurn::exit_plan_request(
                "epm-hanging",
                "# Scripted plan",
            )),
        )
        .await;
    let mock = fixture.mock(&agent).await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "make a plan".to_owned())
        .await
        .expect("send plan request message");
    let request = fixture
        .expect_paused_tool_request(&agent, "ExitPlanMode")
        .await;
    fixture.approve_exit_plan_mode(&agent, &request).await;

    fixture
        .next_chat_event_matching(&agent, "continuation exhaustion card", |event| {
            matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Error)
                        && message.content.contains("mock backend script exhausted")
            )
        })
        .await;
    let env = fixture
        .next_frame_matching("fatal AgentError after continuation exhaustion", |env| {
            env.kind == FrameKind::AgentError
                && env.stream == agent.stream
                && env
                    .parse_payload::<AgentErrorPayload>()
                    .is_ok_and(|payload| payload.fatal)
        })
        .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(
        payload.fatal,
        "continuation exhaustion must close the backend"
    );

    let violations = mock.violations().await;
    assert!(
        matches!(&violations[..], [MockViolation::ScriptExhausted { .. }]),
        "terminal report must record the continuation exhaustion: {violations:?}"
    );
}

#[tokio::test]
async fn reserved_script_governs_resumed_session() {
    let mut fixture = Fixture::new().await;
    let (seed, seed_start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("resume-seed".to_owned()),
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
        .await;
    fixture.finish_turn(&seed).await;
    let session_id = seed_start
        .session_id
        .expect("seed AgentStart carries the session id");

    let reservation = fixture
        .reserve_next_mock_launch(
            "resume-scripted",
            MockScript::one(MockTurn::text("scripted resume response")),
        )
        .await;
    let (resumed, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("resume-scripted".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some("kick".to_owned()),
            },
        })
        .await;
    drop(reservation);

    let turn = fixture.finish_turn(&resumed).await;
    turn.assert_stream_end_contains("scripted resume response");
    fixture.mock(&resumed).await.assert_clean().await;
}

#[tokio::test]
async fn reserved_script_governs_forked_session() {
    let mut fixture = Fixture::new().await;
    let (seed, seed_start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("fork-seed".to_owned()),
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
        .await;
    fixture.finish_turn(&seed).await;
    let session_id = seed_start
        .session_id
        .expect("seed AgentStart carries the session id");

    let reservation = fixture
        .reserve_next_mock_launch(
            "fork-scripted",
            MockScript::one(MockTurn::text("scripted fork response")),
        )
        .await;
    let (forked, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("fork-scripted".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: session_id,
                prompt: "fork prompt".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await;
    drop(reservation);

    let turn = fixture.finish_turn(&forked).await;
    turn.assert_stream_end_contains("scripted fork response");
    fixture.mock(&forked).await.assert_clean().await;
}

#[tokio::test]
async fn explicit_unbounded_echo_default_serves_every_turn() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "echo-default",
            MockScript::one(MockTurn::text("scripted launch response"))
                .with_unbounded_echo()
                .with_user_bubbles(),
        )
        .await;
    fixture.finish_turn(&agent).await;

    for message in ["first free input", "second free input"] {
        fixture
            .client
            .send_message(&agent.stream, message.to_owned())
            .await
            .expect("send unscripted message");
        fixture
            .next_chat_event_matching(&agent, "echoed user bubble", |event| {
                matches!(
                    event,
                    ChatEvent::MessageAdded(bubble)
                        if matches!(bubble.sender, MessageSender::User)
                            && bubble.content == message
                )
            })
            .await;
        let turn = fixture.finish_turn(&agent).await;
        turn.assert_stream_end_contains(&format!("mock backend response to: {message}"));
    }

    fixture.mock(&agent).await.assert_clean().await;
}

#[tokio::test]
async fn history_join_renders_recorded_prompts_at_execution() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "history-render",
            MockScript::one(MockTurn::text("launch response")).then(MockTurn::history_join()),
        )
        .await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "alpha".to_owned())
        .await
        .expect("send history request");
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("mock history: scripted launch | alpha");
    fixture.mock(&agent).await.assert_clean().await;
}

#[tokio::test]
async fn cancelled_turn_emits_operation_cancelled() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "cancelling",
            MockScript::one(MockTurn::text("launch response"))
                .then(MockTurn::cancelled("mock backend cancelled: stop it")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "stop it".to_owned())
        .await
        .expect("send cancelled-turn message");
    fixture
        .next_chat_event_matching(&agent, "operation cancelled", |event| {
            matches!(
                event,
                ChatEvent::OperationCancelled(data)
                    if data.message == "mock backend cancelled: stop it"
            )
        })
        .await;
    fixture
        .next_chat_event_matching(&agent, "idle after cancellation", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    fixture.mock(&agent).await.assert_clean().await;
}

#[tokio::test]
async fn busy_then_close_stream_dies_on_release() {
    let mut fixture = Fixture::new().await;
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "dying",
            MockScript::one(MockTurn::busy_then_close_stream(&gate)),
        )
        .await;
    fixture
        .next_chat_event_matching(&agent, "busy before death", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    gate.wait_until_entered().await;

    gate.release_one();
    let env = fixture
        .next_frame_matching("fatal AgentError after stream close", |env| {
            env.kind == FrameKind::AgentError
                && env.stream == agent.stream
                && env
                    .parse_payload::<AgentErrorPayload>()
                    .is_ok_and(|payload| payload.fatal)
        })
        .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(payload.fatal, "stream close must be a terminal failure");
}

#[tokio::test]
async fn post_idle_builders_append_extra_status_frames() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "extra-idles",
            MockScript::one(MockTurn::text("launch response").with_duplicate_idle())
                .then(MockTurn::text("cycled response").with_active_idle_cycle()),
        )
        .await;
    // The launch turn ends with two idle transitions.
    fixture.finish_turn(&agent).await;
    fixture
        .next_chat_event_matching(&agent, "duplicate idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;

    fixture
        .client
        .send_message(&agent.stream, "cycle".to_owned())
        .await
        .expect("send cycled-turn message");
    // The turn completes, then an extra active→idle cycle follows.
    fixture.finish_turn(&agent).await;
    fixture
        .next_chat_event_matching(&agent, "extra active", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    fixture
        .next_chat_event_matching(&agent, "extra idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    fixture.mock(&agent).await.assert_clean().await;
}

#[tokio::test]
async fn reserved_spawn_failure_fails_startup_visibly() {
    let mut fixture = Fixture::new().await;
    let reservation = fixture
        .reserve_next_mock_spawn_failure("doomed", "mock backend forced spawn failure")
        .await;
    let (agent, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("doomed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "never starts".to_owned(),
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
    let env =
        next_logical_frame_matching_on(&mut fixture.client, "spawn-failure AgentError", |env| {
            env.kind == FrameKind::AgentError && env.stream == agent.stream
        })
        .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(
        payload.fatal
            && payload
                .message
                .contains("mock backend forced spawn failure"),
        "unexpected spawn-failure error: {payload:?}"
    );
}

#[tokio::test]
async fn stream_end_first_exit_plan_gates_the_completion() {
    let mut fixture = Fixture::new().await;
    let before_gate = MockGateHandle::new();
    let after_gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "sef-planner",
            MockScript::one(MockTurn::text("launch response"))
                .then(MockTurn::exit_plan_request_stream_end_first(
                    "epm-sef",
                    "# Plan",
                    &before_gate,
                    &after_gate,
                ))
                .then(MockTurn::text("after approval")),
        )
        .await;
    fixture.finish_turn(&agent).await;

    fixture
        .client
        .send_message(&agent.stream, "plan it".to_owned())
        .await
        .expect("send plan message");
    // Stream-end-first ordering: the closed stream precedes the request.
    let mut saw_stream_end = false;
    fixture
        .next_chat_event_matching(&agent, "tool request after closed stream", |event| {
            if matches!(event, ChatEvent::StreamEnd(_)) {
                saw_stream_end = true;
            }
            matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "epm-sef")
        })
        .await;
    assert!(
        saw_stream_end,
        "the stream must close before the ExitPlanMode request"
    );
    let request = protocol::ToolRequest {
        tool_call_id: "epm-sef".to_owned(),
        tool_name: "exit_plan_mode".to_owned(),
        tool_type: protocol::ToolRequestType::ExitPlanMode {
            plan: Some("# Plan".to_owned()),
            plan_path: Some("/tmp/mock/mock-plan.md".to_owned()),
        },
    };
    fixture.approve_exit_plan_mode(&agent, &request).await;

    // The response parks before the completion frame is emitted...
    before_gate.wait_until_entered().await;
    before_gate.release_one();
    fixture
        .next_chat_event_matching(&agent, "gated ExitPlanMode completion", |event| {
            matches!(
                event,
                ChatEvent::ToolExecutionCompleted(done)
                    if done.tool_call_id == "epm-sef"
                        && fixture::tool_completion_succeeded(done)
            )
        })
        .await;
    // ...and again after it, before the continuation turn.
    after_gate.wait_until_entered().await;
    after_gate.release_one();
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("after approval");
    fixture.mock(&agent).await.assert_clean().await;
}

/// The launch form of the dying backend (`busy_then_close_stream` as the
/// launch turn — the old `__mock_die_after_busy__`) must not record its
/// prompt: a session that dies at launch carries nothing into
/// resumed/replayed history. The resumed session's rendered history proves
/// the boundary state directly — it contains only the resume prompt, never
/// the dying launch prompt. (Later-*input* dying turns still record, as the
/// legacy input form did.)
#[tokio::test]
async fn dying_launch_prompt_is_absent_from_resumed_history() {
    let mut fixture = Fixture::new().await;
    let gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "dying-launch",
            MockScript::one(MockTurn::busy_then_close_stream(&gate)),
        )
        .await;
    let (agent, start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("dying-launch".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "prompt that must not be recorded".to_owned(),
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
    let session_id = start
        .session_id
        .expect("dying-launch AgentStart carries the session id");

    fixture
        .next_chat_event_matching(&agent, "busy before launch death", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    gate.wait_until_entered().await;
    gate.release_one();
    let env = fixture
        .next_frame_matching("fatal AgentError after launch death", |env| {
            env.kind == FrameKind::AgentError
                && env.stream == agent.stream
                && env
                    .parse_payload::<AgentErrorPayload>()
                    .is_ok_and(|payload| payload.fatal)
        })
        .await;
    let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
    assert!(payload.fatal, "launch death must be a terminal failure");

    // Resume the dead session with a scripted history turn: the rendered
    // history is the session record's boundary state.
    let resume_reservation = fixture
        .reserve_next_mock_launch(
            "dying-launch-resume",
            MockScript::one(MockTurn::history_join()),
        )
        .await;
    let (resumed, _start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("dying-launch-resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some("show recorded history".to_owned()),
            },
        })
        .await;
    drop(resume_reservation);

    let history = fixture
        .next_chat_event_matching(&resumed, "history after launch death", |event| {
            matches!(
                event,
                ChatEvent::StreamEnd(end) if end.message.content.contains("mock history:")
            )
        })
        .await;
    let ChatEvent::StreamEnd(end) = history else {
        unreachable!("matched StreamEnd above");
    };
    assert!(
        end.message
            .content
            .contains("mock history: show recorded history"),
        "the resumed session's history must hold exactly the resume prompt: {:?}",
        end.message.content
    );
    assert!(
        !end.message
            .content
            .contains("prompt that must not be recorded"),
        "a launch-time death must not record its prompt into replayable history: {:?}",
        end.message.content
    );
}
