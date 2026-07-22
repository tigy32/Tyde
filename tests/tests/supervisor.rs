//! Integration coverage for the agent supervisor: the hidden background
//! verdict that kicks stalled agents and optionally auto-compacts finished
//! ones. Runs entirely on the mock backend — the mock supervision verdict is
//! Continue when an error is in context and Done otherwise.

mod fixture;

use fixture::Fixture;
use protocol::{
    BackendKind, ChatEvent, Envelope, FrameKind, HostSettingValue, HostSettingsPayload,
    MessageSender, NewAgentPayload, SUPERVISOR_MESSAGE_PREFIX, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};
use std::time::Duration;

const MOCK_ERROR_WITHOUT_IDLE_SENTINEL: &str = "__mock_error_without_idle__";
/// Opts the mock backend into emitting `MessageSender::User` transcript
/// bubbles like real backends do — the supervisor's context reader consumes
/// them, so supervised sessions must run with bubbles on.
const MOCK_USER_BUBBLES_SENTINEL: &str = "__mock_user_bubbles__";

/// The supervisor debounces 3s after an idle transition before reading
/// context, so supervisor-driven frames need a longer wait than ordinary
/// turn frames.
const SUPERVISION_WAIT: Duration = Duration::from_secs(20);
/// Bounded window used to assert that supervision did NOT act (kick budget
/// exhausted, post-compaction guard). Longer than debounce + verdict time.
const QUIET_WAIT: Duration = Duration::from_secs(8);

async fn wait_for_envelope(
    client: &mut client::Connection,
    timeout: Duration,
    context: &str,
    mut pred: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for {context}");
        }
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if pred(&env) {
            return env;
        }
    }
}

/// Drains events for `window` and panics if any matches `pred`.
async fn assert_no_envelope(
    client: &mut client::Connection,
    window: Duration,
    context: &str,
    mut pred: impl FnMut(&Envelope) -> bool,
) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => panic!("next_event failed while asserting {context}: {err:?}"),
            Err(_) => return,
        };
        assert!(!pred(&env), "unexpected {context}: {env:?}");
    }
}

fn chat_event_on(env: &Envelope, stream: &StreamPath) -> Option<ChatEvent> {
    if env.kind != FrameKind::ChatEvent || env.stream != *stream {
        return None;
    }
    env.parse_payload::<ChatEvent>().ok()
}

fn is_supervisor_kick(env: &Envelope, stream: &StreamPath) -> bool {
    matches!(
        chat_event_on(env, stream),
        Some(ChatEvent::MessageAdded(message))
            if matches!(message.sender, MessageSender::User)
                && message.content.starts_with(SUPERVISOR_MESSAGE_PREFIX)
    )
}

fn is_assistant_message_containing(env: &Envelope, stream: &StreamPath, needle: &str) -> bool {
    match chat_event_on(env, stream) {
        Some(ChatEvent::MessageAdded(message)) => {
            matches!(message.sender, MessageSender::Assistant { .. })
                && message.content.contains(needle)
        }
        Some(ChatEvent::StreamEnd(data)) => {
            matches!(data.message.sender, MessageSender::Assistant { .. })
                && data.message.content.contains(needle)
        }
        _ => false,
    }
}

async fn apply_supervisor_setting(fixture: &mut Fixture, setting: HostSettingValue) {
    fixture
        .client
        .set_setting(SetSettingPayload { setting })
        .await
        .expect("send SetSetting");
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "HostSettings after supervisor SetSetting",
        |env| env.kind == FrameKind::HostSettings,
    )
    .await;
}

async fn spawn_supervised_agent(fixture: &mut Fixture, name: &str) -> StreamPath {
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: format!("hello {MOCK_USER_BUBBLES_SENTINEL}"),
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

    let env = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "NewAgent",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let stream = agent_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "initial mock turn",
        |env| is_assistant_message_containing(env, &stream, "mock backend response to: hello"),
    )
    .await;
    agent_stream
}

/// Failure mode 1: a backend error card halts the turn. With the supervisor
/// enabled, the idle agent must receive a visible supervisor-prefixed kick
/// (the mock verdict is Continue because an error is in the context) which
/// starts a real follow-up turn — and the kick budget must stop the loop.
#[tokio::test]
async fn supervisor_kicks_agent_after_error_and_respects_kick_budget() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorMaxKicksPerTask { count: 1 },
    )
    .await;

    let agent_stream = spawn_supervised_agent(&mut fixture, "supervised-error-agent").await;

    fixture
        .client
        .send_message(&agent_stream, MOCK_ERROR_WITHOUT_IDLE_SENTINEL.to_owned())
        .await
        .expect("send error sentinel failed");

    // The supervisor sees the error, kicks the agent, and the kick runs a
    // real turn (the mock echoes the kick text back).
    let stream = agent_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "supervisor kick message",
        |env| is_supervisor_kick(env, &stream),
    )
    .await;
    let stream = agent_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "turn started by the supervisor kick",
        |env| {
            is_assistant_message_containing(
                env,
                &stream,
                &format!("mock backend response to: {SUPERVISOR_MESSAGE_PREFIX}"),
            )
        },
    )
    .await;

    // The error is still the latest signal after the kicked turn, so the
    // supervisor would kick again — but max_kicks_per_task=1 forbids it.
    let stream = agent_stream.clone();
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "second supervisor kick beyond the budget",
        |env| is_supervisor_kick(env, &stream),
    )
    .await;
}

/// Failure-free path: the supervisor confirms the task is done and, with
/// auto-compact enabled, rotates the agent through compaction exactly once.
/// The replacement agent's bootstrap turn must NOT be supervised into
/// another compaction (that would loop forever).
#[tokio::test]
async fn supervisor_done_verdict_auto_compacts_once() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true },
    )
    .await;

    let _agent_stream = spawn_supervised_agent(&mut fixture, "supervised-done-agent").await;

    // Idle with no error → mock verdict Done → auto-compaction spawns a
    // replacement agent and closes the original.
    let env = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "replacement NewAgent from supervisor auto-compaction",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
    let replacement: NewAgentPayload = env.parse_payload().expect("parse replacement NewAgent");
    assert_eq!(
        replacement.name, "supervised-done-agent",
        "the compacted replacement keeps the original agent name"
    );
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "AgentClosed for the compacted original agent",
        |env| env.kind == FrameKind::AgentClosed,
    )
    .await;

    // The replacement idles after digesting its bootstrap summary. The
    // post-compaction guard must keep the supervisor from compacting again.
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "second auto-compaction of the replacement agent",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

/// Settings sanity over the wire: every supervisor knob committed through
/// SetSetting must round-trip into the fanned-out HostSettings payload.
#[tokio::test]
async fn supervisor_settings_round_trip_over_the_wire() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;

    for setting in [
        HostSettingValue::SupervisorEnabled { enabled: true },
        HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true },
        HostSettingValue::SupervisorMaxKicksPerTask { count: 7 },
        HostSettingValue::SupervisorRetryAttempts { count: 2 },
    ] {
        apply_supervisor_setting(&mut fixture, setting).await;
    }

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::SupervisorRetryAttempts { count: 3 },
        })
        .await
        .expect("send final SetSetting");
    let env = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "final HostSettings fan-out",
        |env| {
            env.kind == FrameKind::HostSettings
                && env
                    .parse_payload::<HostSettingsPayload>()
                    .is_ok_and(|payload| payload.settings.supervisor.retry_attempts == 3)
        },
    )
    .await;
    let payload: HostSettingsPayload = env.parse_payload().expect("parse HostSettings");
    assert!(payload.settings.supervisor.enabled);
    assert!(payload.settings.supervisor.auto_compact_on_success);
    assert_eq!(payload.settings.supervisor.max_kicks_per_task, 7);
    assert_eq!(payload.settings.supervisor.retry_attempts, 3);
}
