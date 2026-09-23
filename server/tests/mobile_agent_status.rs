//! Agent liveness for the lazy (mobile) client, over the real protocol from a
//! real mobile-origin connection. Mobile attaches no agent stream until the
//! user opens an agent, so the host stream itself must say which agents are
//! running: in the `HostBootstrap` descriptors on connect, in `NewAgent` for
//! agents spawned later, and as `AgentTurnStateNotify` when an unattached
//! agent's turn starts or ends. Once the client attaches an agent, that
//! agent's own stream is the only source (issue #61).
mod fixture;

use fixture::{Fixture, TestAgent, next_frame_matching_on, send_load_agent_on};
use protocol::{
    AgentBootstrapPayload, AgentId, AgentTurnStateNotifyPayload, BackendKind, ChatEvent, Envelope,
    FrameKind, NewAgentPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

const DEVICE_ID: &str = "liveness-phone";

fn descriptor<'a>(bootstrap: &'a [NewAgentPayload], agent: &TestAgent) -> &'a NewAgentPayload {
    bootstrap
        .iter()
        .find(|entry| entry.agent_id == agent.new_agent.agent_id)
        .unwrap_or_else(|| {
            panic!(
                "mobile HostBootstrap is missing agent {}",
                agent.new_agent.name
            )
        })
}

fn turn_state_for(env: &Envelope, agent_id: &AgentId) -> Option<bool> {
    if env.kind != FrameKind::AgentTurnStateNotify {
        return None;
    }
    let payload: AgentTurnStateNotifyPayload = env
        .parse_payload()
        .expect("parse AgentTurnStateNotifyPayload");
    (payload.agent_id == *agent_id).then_some(payload.turn_active)
}

/// The next host-stream liveness update for `agent_id`. A lazy client that
/// has not attached `agent_id` must never see that agent's stream, and an
/// attached agent must never be announced on the host stream, so both are
/// rejected while waiting.
async fn next_turn_state_on(
    client: &mut client::Connection,
    agent_id: &AgentId,
    attached: &[StreamPath],
    context: &str,
) -> bool {
    let mut turn_active = None;
    next_frame_matching_on(client, context, |env| {
        assert!(
            !env.stream.0.starts_with("/agent/") || attached.contains(&env.stream),
            "lazy client received {} on {} without attaching it (waiting for {context})",
            env.kind,
            env.stream
        );
        if env.kind == FrameKind::AgentTurnStateNotify {
            let payload: AgentTurnStateNotifyPayload = env
                .parse_payload()
                .expect("parse AgentTurnStateNotifyPayload");
            assert!(
                !attached.iter().any(|stream| stream
                    .0
                    .starts_with(&format!("/agent/{}/", payload.agent_id))),
                "host stream announced liveness for attached agent {} (waiting for {context})",
                payload.agent_id
            );
        }
        turn_state_for(env, agent_id)
            .inspect(|state| turn_active = Some(*state))
            .is_some()
    })
    .await;
    turn_active.expect("matched AgentTurnStateNotify")
}

/// Drain the desktop client until `agent`'s current turn ends. The desktop
/// client is eager, so it also drops other agents' frames while spawning, and
/// the actor records the turn as completed before it emits this marker — so
/// once it arrives, the host-side liveness has already settled.
async fn settle_turn(fixture: &mut Fixture, agent: &TestAgent) {
    fixture
        .next_chat_event_matching(agent, "turn idle marker", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
}

async fn call_control_tool(
    url: &str,
    authorization: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("agent-control HTTP client")
        .post(url)
        .header("Authorization", authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .send()
        .await
        .expect("agent-control HTTP request")
        .error_for_status()
        .expect("agent-control HTTP status")
        .text()
        .await
        .expect("agent-control HTTP response");
    let payload = response
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("agent-control SSE data");
    let response: serde_json::Value = serde_json::from_str(payload).expect("agent-control JSON");
    assert_eq!(response["result"]["isError"].as_bool(), Some(false));
    serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool content"),
    )
    .expect("tool JSON")
}

#[tokio::test]
async fn lazy_client_learns_agent_liveness_from_the_host_stream() {
    let mut fixture = Fixture::new().await;

    // `busy` parks mid-turn on a gate, so it is provably still running when
    // the phone connects; `idle` has finished its only turn by then.
    let busy_launch = MockGateHandle::new();
    let busy_follow_up = MockGateHandle::new();
    let busy = fixture
        .spawn_scripted(
            "busy",
            MockScript::one(MockTurn::gated_text("launch turn", &busy_launch))
                .then(MockTurn::gated_text("follow-up turn", &busy_follow_up)),
        )
        .await;
    busy_launch.wait_until_entered().await;

    let idle_follow_up = MockGateHandle::new();
    let idle_second_follow_up = MockGateHandle::new();
    let idle = fixture
        .spawn_scripted(
            "idle",
            MockScript::one(MockTurn::text("done"))
                .then(MockTurn::gated_text("working again", &idle_follow_up))
                .then(MockTurn::gated_text(
                    "working once more",
                    &idle_second_follow_up,
                )),
        )
        .await;
    settle_turn(&mut fixture, &idle).await;

    // On connect the descriptors already carry each agent's liveness, with no
    // agent stream attached and no AgentBootstrap in flight.
    let (mut mobile, bootstrap) =
        fixture::connect_mobile_client_with_bootstrap(fixture.host_for_test(), DEVICE_ID).await;
    let busy_descriptor = descriptor(&bootstrap.agents, &busy);
    let idle_descriptor = descriptor(&bootstrap.agents, &idle);
    assert!(
        busy_descriptor.turn_active,
        "an agent mid-turn must be listed as running on connect"
    );
    assert!(
        !idle_descriptor.turn_active,
        "an agent between turns must be listed as idle on connect"
    );
    let mobile_busy_stream = busy_descriptor.instance_stream.clone();

    // The running agent finishes: the phone learns it went idle without ever
    // attaching the agent.
    busy_launch.release_one();
    settle_turn(&mut fixture, &busy).await;
    assert!(
        !next_turn_state_on(
            &mut mobile,
            &busy.new_agent.agent_id,
            &[],
            "busy going idle"
        )
        .await,
        "busy must be announced idle after its turn ends"
    );

    // The idle agent starts a new turn and finishes it: both edges arrive.
    fixture
        .client
        .send_message(&idle.stream, "again".to_owned())
        .await
        .expect("send follow-up to idle");
    idle_follow_up.wait_until_entered().await;
    assert!(
        next_turn_state_on(
            &mut mobile,
            &idle.new_agent.agent_id,
            &[],
            "idle going busy"
        )
        .await,
        "idle must be announced running once its follow-up turn starts"
    );
    idle_follow_up.release_one();
    settle_turn(&mut fixture, &idle).await;
    assert!(
        !next_turn_state_on(
            &mut mobile,
            &idle.new_agent.agent_id,
            &[],
            "idle going idle"
        )
        .await,
        "idle must be announced idle again after its follow-up turn ends"
    );

    // An agent spawned from the phone after connect is described running from
    // the start, then its auto-opened stream bootstraps with the same state.
    let late_launch = MockGateHandle::new();
    let late_reservation = fixture
        .reserve_next_mock_launch(
            "late",
            MockScript::one(MockTurn::gated_text("late launch", &late_launch)),
        )
        .await;
    mobile
        .spawn_agent(SpawnAgentPayload {
            name: Some("late".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "started from the phone".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn late agent from mobile");
    late_launch.wait_until_entered().await;
    let late_new_agent: NewAgentPayload =
        next_frame_matching_on(&mut mobile, "late NewAgent", |env| {
            assert!(
                !env.stream.0.starts_with("/agent/"),
                "lazy client received {} on {} without attaching it",
                env.kind,
                env.stream
            );
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|payload| payload.name == "late")
        })
        .await
        .parse_payload()
        .expect("parse late NewAgentPayload");
    assert!(
        late_new_agent.turn_active,
        "an agent spawned by the phone must be described as running"
    );
    let late_stream = late_new_agent.instance_stream.clone();
    send_load_agent_on(&mut mobile, &late_stream).await;
    let late_bootstrap: AgentBootstrapPayload =
        next_frame_matching_on(&mut mobile, "late AgentBootstrap", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == late_stream
        })
        .await
        .parse_payload()
        .expect("parse late AgentBootstrapPayload");
    assert!(
        late_bootstrap.turn_active,
        "the phone-opened agent stream must bootstrap as running"
    );
    late_launch.release_one();
    drop(late_reservation);
    next_frame_matching_on(&mut mobile, "late idle on its own stream", |env| {
        env.stream == late_stream
            && env.kind == FrameKind::ChatEvent
            && matches!(
                env.parse_payload::<ChatEvent>(),
                Ok(ChatEvent::TypingStatusChanged(false))
            )
    })
    .await;

    // Opening an agent attaches its stream: the AgentBootstrap is
    // authoritative, and from then on its liveness travels on that stream
    // only — never again as a host-stream notify.
    send_load_agent_on(&mut mobile, &mobile_busy_stream).await;
    let bootstrap_env = next_frame_matching_on(&mut mobile, "busy AgentBootstrap", |env| {
        env.kind == FrameKind::AgentBootstrap && env.stream == mobile_busy_stream
    })
    .await;
    let busy_bootstrap: AgentBootstrapPayload = bootstrap_env
        .parse_payload()
        .expect("parse busy AgentBootstrapPayload");
    assert!(
        !busy_bootstrap.turn_active,
        "AgentBootstrap must report busy idle between turns"
    );
    let attached = [mobile_busy_stream.clone()];

    mobile
        .send_message(&mobile_busy_stream, "again".to_owned())
        .await
        .expect("send follow-up to idle agent from mobile");
    busy_follow_up.wait_until_entered().await;
    let typing_on_stream = |env: &Envelope, expected: bool| {
        env.stream == mobile_busy_stream
            && env.kind == FrameKind::ChatEvent
            && matches!(
                env.parse_payload::<ChatEvent>(),
                Ok(ChatEvent::TypingStatusChanged(active)) if active == expected
            )
    };
    next_frame_matching_on(&mut mobile, "busy typing on its own stream", |env| {
        assert!(
            turn_state_for(env, &busy.new_agent.agent_id).is_none(),
            "attached agent busy must not be announced on the host stream"
        );
        typing_on_stream(env, true)
    })
    .await;
    busy_follow_up.release_one();
    settle_turn(&mut fixture, &busy).await;
    next_frame_matching_on(&mut mobile, "busy idle on its own stream", |env| {
        assert!(
            turn_state_for(env, &busy.new_agent.agent_id).is_none(),
            "attached agent busy must not be announced on the host stream"
        );
        typing_on_stream(env, false)
    })
    .await;

    // Meanwhile an unattached agent still reports on the host stream, and the
    // attached one still never leaks there.
    fixture
        .client
        .send_message(&idle.stream, "once more".to_owned())
        .await
        .expect("send second follow-up to idle");
    // With only two scripted turns this request emitted ScriptExhausted and
    // closed the backend; observing its transient running flag was a race.
    idle_second_follow_up.wait_until_entered().await;
    assert!(
        next_turn_state_on(
            &mut mobile,
            &idle.new_agent.agent_id,
            &attached,
            "idle busy again"
        )
        .await,
        "unattached idle must still be announced running on the host stream"
    );
    idle_second_follow_up.release_one();
    assert!(
        !next_turn_state_on(
            &mut mobile,
            &idle.new_agent.agent_id,
            &attached,
            "idle second follow-up finished"
        )
        .await,
        "unattached idle must return to idle after its second follow-up"
    );

    let question_gate = MockGateHandle::new();
    let question_follow_up = MockGateHandle::new();
    let answer_continuation = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "async-question",
            MockScript::one(MockTurn::async_question_request(
                "liveness-question",
                &question_gate,
            ))
            .then(MockTurn::gated_text(
                "independent follow-up",
                &question_follow_up,
            ))
            .then(MockTurn::gated_text(
                "answer continuation",
                &answer_continuation,
            ))
            .then(MockTurn::text("queued after late answer")),
        )
        .await;
    let (question_agent, _) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("async-question".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(busy.new_agent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "scripted launch".to_owned(),
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
    question_gate.wait_until_entered().await;
    let (mut question_mobile, bootstrap) =
        fixture::connect_mobile_client_with_bootstrap(fixture.host_for_test(), "question-phone")
            .await;
    assert!(
        descriptor(&bootstrap.agents, &question_agent).turn_active,
        "pending nonblocking question must not hide continuing work"
    );
    question_gate.release_one();
    let request = fixture
        .expect_paused_tool_request(&question_agent, "AskUserQuestion")
        .await;
    assert!(matches!(
        request.tool_type,
        protocol::ToolRequestType::AskUserQuestion {
            mode: protocol::UserQuestionMode::NonBlocking,
            ..
        }
    ));
    let (mut late_mobile, bootstrap) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "late-question-phone",
    )
    .await;
    assert!(
        !descriptor(&bootstrap.agents, &question_agent).turn_active,
        "ended nonblocking-question turn must be idle in HostBootstrap while its answer remains pending"
    );
    assert!(
        !next_turn_state_on(
            &mut question_mobile,
            &question_agent.new_agent.agent_id,
            &[],
            "unanswered question turn ended"
        )
        .await
    );
    let late_stream = descriptor(&bootstrap.agents, &question_agent)
        .instance_stream
        .clone();
    send_load_agent_on(&mut late_mobile, &late_stream).await;
    let bootstrap: AgentBootstrapPayload =
        next_frame_matching_on(&mut late_mobile, "idle question bootstrap", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == late_stream
        })
        .await
        .parse_payload()
        .expect("parse question bootstrap");
    assert!(
        !bootstrap.turn_active,
        "pending card must not keep AgentBootstrap active"
    );
    assert!(bootstrap.events.iter().any(|event| matches!(event,
        protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(pending))
            if pending.tool_call_id == request.tool_call_id)));
    assert!(!bootstrap.events.iter().any(|event| matches!(event,
        protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
            if completion.tool_call_id == request.tool_call_id)));
    let caller = fixture.agent_control_caller(&busy.new_agent.agent_id).await;
    let agents = call_control_tool(
        &caller.url,
        &caller.authorization,
        "tyde_list_agents",
        serde_json::json!({}),
    )
    .await;
    let listed = agents
        .as_array()
        .expect("agent list")
        .iter()
        .find(|entry| {
            entry["agent_id"].as_str() == Some(question_agent.new_agent.agent_id.0.as_str())
        })
        .expect("question agent listed");
    assert_eq!(listed["status"].as_str(), Some("idle"));

    fixture
        .client
        .send_message(&question_agent.stream, "independent follow-up".to_owned())
        .await
        .expect("send follow-up without answering");
    question_follow_up.wait_until_entered().await;
    assert!(
        next_turn_state_on(
            &mut question_mobile,
            &question_agent.new_agent.agent_id,
            &[],
            "independent follow-up active"
        )
        .await
    );
    let agents = call_control_tool(
        &caller.url,
        &caller.authorization,
        "tyde_list_agents",
        serde_json::json!({}),
    )
    .await;
    let listed = agents
        .as_array()
        .expect("agent list")
        .iter()
        .find(|entry| {
            entry["agent_id"].as_str() == Some(question_agent.new_agent.agent_id.0.as_str())
        })
        .expect("question agent listed during follow-up");
    eprintln!(
        "Async follow-up control status: thinking={}, pending_card=true",
        listed["status"] == "thinking"
    );
    let awaiting = call_control_tool(
        &caller.await_url,
        &caller.authorization,
        "tyde_await_agents",
        serde_json::json!({
            "agent_ids": [question_agent.new_agent.agent_id]
        }),
    );
    tokio::pin!(awaiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut awaiting)
            .await
            .is_err(),
        "agent-control await must not report ready during gated work with an unanswered async card"
    );
    assert_eq!(
        listed["status"].as_str(),
        Some("thinking"),
        "agent-control must report Thinking during independent work with an unanswered async card"
    );
    question_follow_up.release_one();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), awaiting)
        .await
        .expect("await completes after terminal idle");
    assert_eq!(ready["ready"].as_array().map(Vec::len), Some(1));
    assert_eq!(ready["ready"][0]["status"], "idle");
    assert_eq!(ready["still_thinking"].as_array().map(Vec::len), Some(0));

    let follow_up = fixture.finish_turn(&question_agent).await;
    assert!(follow_up.chat_events().iter().any(|event| matches!(event,
        ChatEvent::StreamEnd(end) if end.message.content == "independent follow-up")));
    assert!(!follow_up.chat_events().iter().any(|event| matches!(event,
        ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == request.tool_call_id)));
    assert!(
        !next_turn_state_on(
            &mut question_mobile,
            &question_agent.new_agent.agent_id,
            &[],
            "independent follow-up idle"
        )
        .await
    );

    let answer_payload = protocol::SendMessagePayload {
        message: "GREEN".to_owned(),
        images: None,
        origin: None,
        tool_response: Some(protocol::SendMessageToolResponse::AskUserQuestion {
            tool_call_id: request.tool_call_id.clone(),
            answer: "GREEN".to_owned(),
        }),
    };
    fixture
        .client
        .send_message_payload(&question_agent.stream, answer_payload.clone())
        .await
        .expect("answer idle question");
    fixture
        .client
        .send_message(
            &question_agent.stream,
            "queued after late answer".to_owned(),
        )
        .await
        .expect("send immediately after late answer");
    answer_continuation.wait_until_entered().await;
    assert!(
        next_turn_state_on(
            &mut question_mobile,
            &question_agent.new_agent.agent_id,
            &[],
            "late answer active"
        )
        .await
    );
    // Gate entry proves the answer started, not that the following client
    // message arrived. Releasing before its queue acknowledgement lets that
    // message legitimately arrive after idle instead of racing active work.
    let mut answered = fixture::Turn { frames: Vec::new() };
    next_frame_matching_on(&mut fixture.client, "late-answer follow-up queued", |env| {
        if env.stream != question_agent.stream {
            return false;
        }
        let queued = env.kind == FrameKind::QueuedMessages
            && env
                .parse_payload::<protocol::QueuedMessagesPayload>()
                .is_ok_and(|payload| payload.messages.len() == 1);
        answered.frames.push(env.clone());
        queued
    })
    .await;
    eprintln!(
        "Async answer gate held through queue acknowledgement; observed_frames={}, busy_markers={}, idle_markers={}",
        answered.frames.len(),
        answered
            .chat_events()
            .iter()
            .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
            .count(),
        answered
            .chat_events()
            .iter()
            .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
            .count()
    );
    answer_continuation.release_one();
    // The queue acknowledgement already consumed this turn's busy markers.
    // Starting finish_turn here can consume the following turn before returning.
    next_frame_matching_on(
        &mut fixture.client,
        "late-answer continuation idle",
        |env| {
            if env.stream != question_agent.stream {
                return false;
            }
            let idle = env.kind == FrameKind::ChatEvent
                && matches!(
                    env.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::TypingStatusChanged(false))
                );
            answered.frames.push(env.clone());
            idle
        },
    )
    .await;
    let events = answered.chat_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
    );
    assert_eq!(events.iter().filter(|event| matches!(event,
        ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == request.tool_call_id
            && matches!(completion.outcome, protocol::ToolExecutionOutcome::Succeeded { .. }))).count(), 1,
        "late answer must complete its card exactly once");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
        ChatEvent::MessageAdded(message) if matches!(message.sender, protocol::MessageSender::User)
            && message.content == "GREEN"))
            .count(),
        1,
        "answer must be persisted exactly once"
    );
    assert!(
        answered
            .queued_message_snapshots()
            .iter()
            .any(|snapshot| snapshot.messages.len() == 1),
        "message racing the late answer must queue behind its continuation"
    );
    let queued = fixture.finish_turn(&question_agent).await;
    assert!(queued.chat_events().iter().any(|event| matches!(event,
        ChatEvent::StreamEnd(end) if end.message.content == "queued after late answer")));
    assert!(!queued.chat_events().iter().any(|event| matches!(event,
        ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == request.tool_call_id)));
    fixture
        .client
        .send_message_payload(&question_agent.stream, answer_payload)
        .await
        .expect("send duplicate answer");
    fixture.next_chat_event_matching(&question_agent, "duplicate answer rejected", |event|
        matches!(event, ChatEvent::MessageAdded(message) if matches!(message.sender, protocol::MessageSender::Error))
    ).await;
    let (mut final_mobile, bootstrap) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "answered-question-phone",
    )
    .await;
    assert!(
        !descriptor(&bootstrap.agents, &question_agent).turn_active,
        "duplicate answer must not resurrect an ended turn"
    );
    let final_stream = descriptor(&bootstrap.agents, &question_agent)
        .instance_stream
        .clone();
    send_load_agent_on(&mut final_mobile, &final_stream).await;
    let bootstrap: AgentBootstrapPayload =
        next_frame_matching_on(&mut final_mobile, "answered question bootstrap", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == final_stream
        })
        .await
        .parse_payload()
        .expect("parse answered bootstrap");
    assert!(!bootstrap.turn_active);
    assert_eq!(
        bootstrap
            .events
            .iter()
            .filter(|event| matches!(event,
        protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
            if completion.tool_call_id == request.tool_call_id))
            .count(),
        1
    );
    fixture.mock(&question_agent).await.assert_clean().await;
}
