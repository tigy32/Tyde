mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorPayload, AgentStartPayload, BackendKind,
    CancelQueuedMessagePayload, ChatEvent, Envelope, FrameKind, NewAgentPayload, QueuedMessageId,
    QueuedMessagesPayload, SendMessagePayload, SendMessageToolResponse,
    SendQueuedMessageNowPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath, ToolRequest,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn raw_next(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

/// Wait for the first `QueuedMessages` frame that has exactly `count` entries.
/// All other frame kinds are skipped.
async fn expect_queued_messages_with_count(
    client: &mut client::Connection,
    count: usize,
    context: &str,
) -> QueuedMessagesPayload {
    loop {
        let env = raw_next(client, context).await;
        if env.kind == FrameKind::QueuedMessages {
            let payload: QueuedMessagesPayload =
                env.parse_payload().expect("parse QueuedMessagesPayload");
            if payload.messages.len() == count {
                return payload;
            }
        }
        if env.kind == FrameKind::AgentBootstrap {
            let payload: AgentBootstrapPayload =
                env.parse_payload().expect("parse AgentBootstrapPayload");
            for event in payload.events {
                if let AgentBootstrapEvent::QueuedMessages(payload) = event
                    && payload.messages.len() == count
                {
                    return payload;
                }
            }
        }
    }
}

/// Wait for the next non-noise event, skipping routine control-plane frames.
async fn skip_noise(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = raw_next(client, context).await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let payload: AgentBootstrapPayload =
                env.parse_payload().expect("parse AgentBootstrapPayload");
            for event in payload.events {
                if let AgentBootstrapEvent::AgentStart(payload) = event {
                    return Envelope::from_payload(
                        env.stream.clone(),
                        FrameKind::AgentStart,
                        env.seq,
                        &payload,
                    )
                    .expect("serialize AgentStart");
                }
            }
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
        ) {
            continue;
        }
        return env;
    }
}

/// Spawn a mock agent and return `(agent_stream, agent_id)` after consuming
/// `NewAgent` and `AgentStart`.
async fn spawn_and_start(client: &mut client::Connection, name: &str, prompt: &str) -> StreamPath {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
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

    let env = skip_noise(client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
    let agent_stream = new_agent.instance_stream.clone();

    let env = skip_noise(client, "AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let _: AgentStartPayload = env.parse_payload().expect("parse AgentStartPayload");

    agent_stream
}

/// Wait for `TypingStatusChanged(true)` on the agent stream, consuming
/// everything that comes before it.
async fn wait_for_typing_true(client: &mut client::Connection, agent_stream: &StreamPath) {
    loop {
        let env = raw_next(client, "TypingStatusChanged(true)").await;
        if env.kind == FrameKind::ChatEvent && env.stream == *agent_stream {
            let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
            if matches!(event, ChatEvent::TypingStatusChanged(true)) {
                return;
            }
        }
    }
}

async fn wait_for_chat_event<F>(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    context: &str,
    mut predicate: F,
) -> ChatEvent
where
    F: FnMut(&ChatEvent) -> bool,
{
    loop {
        let env = raw_next(client, context).await;
        if env.kind == FrameKind::ChatEvent && env.stream == *agent_stream {
            let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
            if predicate(&event) {
                return event;
            }
        }
    }
}

async fn wait_for_stream_end_containing(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    needle: &str,
    context: &str,
) {
    let expected = needle.to_owned();
    wait_for_chat_event(client, agent_stream, context, |event| {
        matches!(
            event,
            ChatEvent::StreamEnd(end) if end.message.content.contains(&expected)
        )
    })
    .await;
}

async fn assert_queue_not_emptied_before_next_typing_true(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    context: &str,
) {
    loop {
        let env = raw_next(client, context).await;
        if env.stream != *agent_stream {
            continue;
        }
        match env.kind {
            FrameKind::QueuedMessages => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("parse QueuedMessagesPayload");
                assert!(
                    !payload.messages.is_empty(),
                    "stale TypingStatusChanged(false) drained the queue before the next turn became busy"
                );
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                if matches!(event, ChatEvent::TypingStatusChanged(true)) {
                    return;
                }
            }
            _ => {}
        }
    }
}

async fn wait_for_exit_plan_mode_pause(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
) -> ToolRequest {
    let mut tool_request = None;
    let mut saw_pause = false;
    loop {
        let event = wait_for_chat_event(client, agent_stream, "ExitPlanMode pause", |_| true).await;
        match event {
            ChatEvent::ToolRequest(request) => {
                assert_eq!(request.tool_name, "ExitPlanMode");
                assert!(matches!(
                    &request.tool_type,
                    protocol::ToolRequestType::ExitPlanMode { .. }
                ));
                tool_request = Some(request);
            }
            ChatEvent::TypingStatusChanged(false) => saw_pause = true,
            _ => {}
        }
        if saw_pause && let Some(request) = tool_request {
            return request;
        }
    }
}

// ---------------------------------------------------------------------------
// Tool responses bypass the normal queue without starting a stray turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exit_plan_mode_tool_response_resumes_and_drains_queue() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "exit-plan-mode",
        "__mock_exit_plan_mode__",
    )
    .await;

    let request = wait_for_exit_plan_mode_pause(&mut fixture.client, &agent_stream).await;
    let protocol::ToolRequestType::ExitPlanMode { plan, plan_path } = request.tool_type else {
        panic!("expected ExitPlanMode tool request");
    };
    assert_eq!(plan.as_deref(), Some("# Plan\n\nApprove the mock plan."));
    assert_eq!(
        plan_path.as_deref(),
        Some("/tmp/mock/.claude/plans/mock-plan.md")
    );

    fixture
        .client
        .send_message(
            &agent_stream,
            "queued while ExitPlanMode is pending".to_owned(),
        )
        .await
        .expect("send queued message while waiting for ExitPlanMode");
    let queued =
        expect_queued_messages_with_count(&mut fixture.client, 1, "queued during ExitPlanMode")
            .await;
    assert_eq!(
        queued.messages[0].message,
        "queued while ExitPlanMode is pending"
    );

    fixture
        .client
        .send_message_payload(
            &agent_stream,
            SendMessagePayload {
                message: String::new(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                    tool_call_id: request.tool_call_id,
                    decision: protocol::ExitPlanModeDecision::Approve,
                    feedback: None,
                }),
            },
        )
        .await
        .expect("send ExitPlanMode approval");

    let mut saw_completion = false;
    let mut saw_approval = false;
    let mut saw_queue_drained = false;
    let mut saw_queued_response = false;
    let mut saw_final_idle = false;
    loop {
        let env = raw_next(
            &mut fixture.client,
            "ExitPlanMode completion and queue drain",
        )
        .await;
        if env.stream != agent_stream {
            continue;
        }
        match env.kind {
            FrameKind::QueuedMessages => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("parse QueuedMessagesPayload");
                if payload.messages.is_empty() {
                    saw_queue_drained = true;
                }
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                match event {
                    ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_name == "ExitPlanMode" =>
                    {
                        assert!(completion.success);
                        saw_completion = true;
                    }
                    ChatEvent::StreamEnd(end)
                        if end.message.content.contains("mock ExitPlanMode approved") =>
                    {
                        saw_approval = true;
                    }
                    ChatEvent::StreamEnd(end)
                        if end
                            .message
                            .content
                            .contains("mock backend response to: queued while ExitPlanMode") =>
                    {
                        saw_queued_response = true;
                    }
                    ChatEvent::TypingStatusChanged(false) if saw_queued_response => {
                        saw_final_idle = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if saw_completion
            && saw_approval
            && saw_queue_drained
            && saw_queued_response
            && saw_final_idle
        {
            break;
        }
    }
}

#[tokio::test]
async fn stale_tool_response_while_idle_does_not_wedge_follow_up() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    let agent_stream = spawn_and_start(&mut fixture.client, "stale-tool-response", "initial").await;
    wait_for_stream_end_containing(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: initial",
        "initial mock response",
    )
    .await;
    wait_for_chat_event(
        &mut fixture.client,
        &agent_stream,
        "initial idle",
        |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
    )
    .await;

    fixture
        .client
        .send_message_payload(
            &agent_stream,
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
    wait_for_chat_event(
        &mut fixture.client,
        &agent_stream,
        "stale response backend error",
        |event| {
            matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, protocol::MessageSender::Error)
                        && message.content.contains("No matching pending tool request")
            )
        },
    )
    .await;

    fixture
        .client
        .send_message(&agent_stream, "after stale response".to_owned())
        .await
        .expect("send follow-up after stale tool response");
    wait_for_stream_end_containing(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: after stale response",
        "follow-up after stale tool response",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Test 1 — Queue while busy: snapshot grows with each new message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queue_while_busy_snapshot_grows() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    // Spawn with __mock_slow__ so the initial turn lingers for 300 ms,
    // giving us time to send messages that must be queued.
    let agent_stream =
        spawn_and_start(&mut fixture.client, "queue-grow", "__mock_slow__ hello").await;

    // Wait until the agent is definitely in-turn on the server side.
    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    // Send first message while busy — must be queued.
    fixture
        .client
        .send_message(&agent_stream, "queued A".to_owned())
        .await
        .expect("send_message A failed");

    let snapshot1 =
        expect_queued_messages_with_count(&mut fixture.client, 1, "QueuedMessages(1)").await;
    assert_eq!(snapshot1.messages.len(), 1);
    assert_eq!(snapshot1.messages[0].message, "queued A");

    // Send second message while still busy.
    fixture
        .client
        .send_message(&agent_stream, "queued B".to_owned())
        .await
        .expect("send_message B failed");

    let snapshot2 =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages(2)").await;
    assert_eq!(snapshot2.messages.len(), 2);
    assert_eq!(snapshot2.messages[0].message, "queued A");
    assert_eq!(snapshot2.messages[1].message, "queued B");
}

// ---------------------------------------------------------------------------
// Test 2 — FIFO drain on TypingStatusChanged(false)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fifo_drain_on_typing_status_false() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-drain",
        "__mock_slow__ drain-test",
    )
    .await;

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "drain A".to_owned())
        .await
        .expect("send drain A");
    fixture
        .client
        .send_message(&agent_stream, "drain B".to_owned())
        .await
        .expect("send drain B");

    // Confirm both are queued.
    let before =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages(2) before drain")
            .await;
    assert_eq!(before.messages[0].message, "drain A");
    assert_eq!(before.messages[1].message, "drain B");

    // Now the 300 ms slow-turn sleep expires → mock sends TypingStatusChanged(false).
    // The actor pops "drain A" from the front, broadcasts QueuedMessages([drain B]),
    // and dispatches "drain A" to the backend.
    let after =
        expect_queued_messages_with_count(&mut fixture.client, 1, "QueuedMessages(1) after drain")
            .await;
    assert_eq!(
        after.messages.len(),
        1,
        "only B should remain after draining A"
    );
    assert_eq!(after.messages[0].message, "drain B");
}

#[tokio::test]
async fn duplicate_typing_false_drains_only_once() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-duplicate-idle",
        "__mock_slow__ __mock_duplicate_idle__ duplicate-idle-test",
    )
    .await;

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "duplicate A".to_owned())
        .await
        .expect("send duplicate A");
    fixture
        .client
        .send_message(&agent_stream, "duplicate B".to_owned())
        .await
        .expect("send duplicate B");

    let before = expect_queued_messages_with_count(
        &mut fixture.client,
        2,
        "QueuedMessages(2) before duplicate idle",
    )
    .await;
    assert_eq!(before.messages[0].message, "duplicate A");
    assert_eq!(before.messages[1].message, "duplicate B");

    let after_first_idle = expect_queued_messages_with_count(
        &mut fixture.client,
        1,
        "QueuedMessages(1) after first idle",
    )
    .await;
    assert_eq!(after_first_idle.messages[0].message, "duplicate B");

    assert_queue_not_emptied_before_next_typing_true(
        &mut fixture.client,
        &agent_stream,
        "next queued turn should become busy before the queue drains again",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Test 3 — CancelQueuedMessage removes the entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_queued_message_removes_entry() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-cancel",
        "__mock_slow__ cancel-test",
    )
    .await;

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "cancel me".to_owned())
        .await
        .expect("send cancel me");

    let snapshot =
        expect_queued_messages_with_count(&mut fixture.client, 1, "QueuedMessages(1)").await;
    let cancel_id: QueuedMessageId = snapshot.messages[0].id.clone();

    fixture
        .client
        .cancel_queued_message(&agent_stream, CancelQueuedMessagePayload { id: cancel_id })
        .await
        .expect("cancel_queued_message failed");

    let empty =
        expect_queued_messages_with_count(&mut fixture.client, 0, "QueuedMessages(0) after cancel")
            .await;
    assert!(
        empty.messages.is_empty(),
        "queue must be empty after cancel"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — SendQueuedMessageNow moves the target to the front
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_queued_message_now_reorders() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-reorder",
        "__mock_slow__ reorder-test",
    )
    .await;

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "order A".to_owned())
        .await
        .expect("send order A");
    fixture
        .client
        .send_message(&agent_stream, "order B".to_owned())
        .await
        .expect("send order B");

    // Wait until both are in the queue.
    let snapshot_ab =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages([A,B])").await;
    assert_eq!(snapshot_ab.messages[0].message, "order A");
    assert_eq!(snapshot_ab.messages[1].message, "order B");

    let b_id: QueuedMessageId = snapshot_ab.messages[1].id.clone();

    // Promote B to the front.
    fixture
        .client
        .send_queued_message_now(
            &agent_stream,
            SendQueuedMessageNowPayload { id: b_id.clone() },
        )
        .await
        .expect("send_queued_message_now failed");

    // The snapshot must now show [B, A].
    let snapshot_ba =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages([B,A])").await;
    assert_eq!(
        snapshot_ba.messages[0].id, b_id,
        "B must be first after SendQueuedMessageNow"
    );
    assert_eq!(snapshot_ba.messages[0].message, "order B");
    assert_eq!(snapshot_ba.messages[1].message, "order A");
}

// ---------------------------------------------------------------------------
// Test 5 — New subscriber receives the current queue snapshot in replay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queue_replays_to_new_subscriber() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-replay",
        "__mock_slow__ replay-test",
    )
    .await;

    // Get the agent_id so we can locate the right agent on client2.
    // We already consumed NewAgent above; let's get it from the stream path.
    // agent_stream is /agent/<agent_id>/<instance_id>
    let agent_id_str = agent_stream
        .0
        .split('/')
        .nth(2)
        .expect("agent_id in stream path");

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "replay A".to_owned())
        .await
        .expect("send replay A");
    fixture
        .client
        .send_message(&agent_stream, "replay B".to_owned())
        .await
        .expect("send replay B");

    // Confirm queue has 2 entries on client1.
    let snapshot1 =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages(2) on client1")
            .await;
    assert_eq!(snapshot1.messages.len(), 2);

    // Connect a fresh client — it must receive the live queue snapshot as part
    // of the agent event-log replay.
    let mut client2 = fixture.connect().await;

    // Drain client2's stream until we see a QueuedMessages frame with 2 entries.
    // Other replay frames (NewAgent, AgentStart, SessionSettings, ChatEvents) are
    // skipped automatically by the helper.
    let snapshot2 =
        expect_queued_messages_with_count(&mut client2, 2, "QueuedMessages(2) replayed on client2")
            .await;
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

    // Verify same IDs are replayed.
    assert_eq!(snapshot2.messages[0].id, snapshot1.messages[0].id);
    assert_eq!(snapshot2.messages[1].id, snapshot1.messages[1].id);

    // Suppress unused-variable warning.
    let _ = agent_id_str;
}

// ---------------------------------------------------------------------------
// Test 6 — Queue clears when the agent terminates mid-turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queue_cleared_on_agent_termination() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    // __mock_die_after_busy__ causes the mock to send TypingStatusChanged(true),
    // sleep 300 ms, then exit — which closes the events channel and triggers
    // enter_terminal_failure inside the agent actor.
    let agent_stream = spawn_and_start(
        &mut fixture.client,
        "queue-terminate",
        "__mock_die_after_busy__ termination-test",
    )
    .await;

    wait_for_typing_true(&mut fixture.client, &agent_stream).await;

    // Queue two messages while the agent is busy.
    fixture
        .client
        .send_message(&agent_stream, "will be lost A".to_owned())
        .await
        .expect("send lost A");
    fixture
        .client
        .send_message(&agent_stream, "will be lost B".to_owned())
        .await
        .expect("send lost B");

    // Confirm the queue is populated.
    let populated =
        expect_queued_messages_with_count(&mut fixture.client, 2, "QueuedMessages(2) before die")
            .await;
    assert_eq!(populated.messages.len(), 2);

    // After the 300 ms sleep, the mock exits.  The agent actor detects the closed
    // events channel, calls enter_terminal_failure which clears the queue and
    // emits QueuedMessages(empty) followed by a fatal AgentError.
    let cleared =
        expect_queued_messages_with_count(&mut fixture.client, 0, "QueuedMessages(0) on terminate")
            .await;
    assert!(
        cleared.messages.is_empty(),
        "queue must be empty after termination"
    );

    // The fatal AgentError must follow.
    loop {
        let env = raw_next(&mut fixture.client, "fatal AgentError after termination").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentError && env.stream == agent_stream {
            let err: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
            assert!(err.fatal, "termination must produce a fatal AgentError");
            break;
        }
        // Skip any interleaved noise (SessionList, etc.).
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::ChatEvent
        ) {
            continue;
        }
    }
}
