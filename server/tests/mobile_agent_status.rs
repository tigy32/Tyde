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
    let idle = fixture
        .spawn_scripted(
            "idle",
            MockScript::one(MockTurn::text("done"))
                .then(MockTurn::gated_text("working again", &idle_follow_up)),
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
}
