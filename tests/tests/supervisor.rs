//! Integration coverage for the agent supervisor: the hidden background
//! verdict that kicks stalled agents and optionally auto-compacts finished
//! ones. Runs entirely on the mock backend — the mock supervision verdict is
//! Continue when an error is in context, AwaitingUser for its explicit
//! sentinel, and Done for its explicit sentinel or legacy default.

mod fixture;

use fixture::Fixture;
use protocol::{
    AgentClosedPayload, BackendKind, ChatEvent, CommandErrorPayload, Envelope, FrameKind,
    FetchSessionHistoryPayload, HostSettingErrorTarget, HostSettingValue, HostSettingsPayload,
    MessageSender, NewAgentPayload, SUPERVISOR_MESSAGE_PREFIX, SetSettingPayload,
    SpawnAgentParams, SpawnAgentPayload, StreamPath,
};
use std::time::Duration;

const MOCK_ERROR_WITHOUT_IDLE_SENTINEL: &str = "__mock_error_without_idle__";
/// Opts the mock backend into emitting `MessageSender::User` transcript
/// bubbles like real backends do — the supervisor's context reader consumes
/// them, so supervised sessions must run with bubbles on.
const MOCK_USER_BUBBLES_SENTINEL: &str = "__mock_user_bubbles__";
const MOCK_CONTEXT_250K_SENTINEL: &str = "__mock_context_250k__";
const MOCK_SUPERVISOR_DONE: &str = "__mock_supervisor_done__";
const MOCK_SUPERVISOR_AWAITING_USER: &str = "__mock_supervisor_awaiting_user__";
const MOCK_SUPERVISOR_ERROR: &str = "__mock_supervisor_error__";
const MOCK_ACTIVE_IDLE_CYCLE: &str = "__mock_active_idle_cycle__";
const MOCK_CODEX_INTERNAL_ERROR_TAIL: &str = "__mock_codex_internal_error_tail__";

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

fn supervisor_failure_warning(env: &Envelope) -> Option<(StreamPath, String)> {
    match env.parse_payload::<ChatEvent>().ok() {
        Some(ChatEvent::MessageAdded(message))
            if env.kind == FrameKind::ChatEvent
                && matches!(message.sender, MessageSender::Warning)
                && message.content.starts_with(
                    "Supervisor could not verify whether this task was complete after ",
                ) => Some((env.stream.clone(), message.content)),
        _ => None,
    }
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

async fn spawn_supervised_agent(
    fixture: &mut Fixture,
    name: &str,
    report_context: bool,
) -> NewAgentPayload {
    spawn_supervised_agent_with_verdict(
        fixture,
        name,
        report_context,
        MOCK_SUPERVISOR_DONE,
    )
    .await
}

async fn spawn_supervised_agent_with_verdict(
    fixture: &mut Fixture,
    name: &str,
    report_context: bool,
    verdict_sentinel: &str,
) -> NewAgentPayload {
    let context_sentinel = if report_context {
        MOCK_CONTEXT_250K_SENTINEL
    } else {
        ""
    };
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: format!(
                    "hello {MOCK_USER_BUBBLES_SENTINEL} {context_sentinel} {verdict_sentinel}"
                ),
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
    new_agent
}

async fn auto_compaction_fixture(threshold: u64) -> Fixture {
    auto_compaction_fixture_with_delay(threshold, 1).await
}

async fn auto_compaction_fixture_with_delay(threshold: u64, delay_seconds: u32) -> Fixture {
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
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds {
            seconds: delay_seconds,
        },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: threshold },
    )
    .await;
    fixture
}

#[tokio::test]
async fn exhausted_supervisor_failure_warns_once_per_activity_generation() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorRetryAttempts { count: 0 },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;

    let other = spawn_supervised_agent(&mut fixture, "unaffected-supervisor-agent", false).await;
    let affected = spawn_supervised_agent_with_verdict(
        &mut fixture,
        "supervisor-failure-warning",
        false,
        MOCK_SUPERVISOR_ERROR,
    )
    .await;
    let singular = "Supervisor could not verify whether this task was complete after 1 attempt and has stopped retrying. Send a follow-up message if you want the agent to continue.";

    let warning = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "terminal supervisor failure warning",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;
    let (warning_stream, warning_copy) =
        supervisor_failure_warning(&warning).expect("supervisor failure warning payload");
    assert_eq!(warning_stream, affected.instance_stream);
    assert_ne!(warning_stream, other.instance_stream);
    assert_eq!(warning_copy, singular);
    assert!(!warning_copy.contains("mock supervision failure"));
    assert!(!warning_copy.contains("BackendStream"));

    fixture
        .client
        .fetch_session_history(
            &affected.instance_stream,
            FetchSessionHistoryPayload {
                agent_id: affected.agent_id.clone(),
                before_seq: None,
                limit: 100,
            },
        )
        .await
        .expect("fetch affected actor history");
    let history = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "affected actor history",
        |env| {
            env.kind == FrameKind::SessionHistory && env.stream == affected.instance_stream
        },
    )
    .await
    .parse_payload::<protocol::SessionHistoryPayload>()
    .expect("parse affected actor history");
    assert_eq!(
        history
            .events
            .iter()
            .filter(|event| matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Warning)
                        && message.content == singular
            ))
            .count(),
        1
    );

    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "duplicate or cross-stream supervisor failure warning",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;

    fixture
        .client
        .send_message(
            &affected.instance_stream,
            format!("new generation {MOCK_SUPERVISOR_ERROR}"),
        )
        .await
        .expect("send new failing supervision generation");
    let affected_stream = affected.instance_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "new generation assistant turn",
        |env| is_assistant_message_containing(env, &affected_stream, "new generation"),
    )
    .await;
    let second = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "new generation supervisor failure warning",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;
    assert_eq!(
        supervisor_failure_warning(&second)
            .expect("second supervisor failure warning")
            .0,
        affected.instance_stream
    );
}

#[tokio::test]
async fn transient_supervisor_failure_and_closed_agent_remain_silent() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorRetryAttempts { count: 1 },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;
    let transient = spawn_supervised_agent_with_verdict(
        &mut fixture,
        "transient-supervisor-failure",
        false,
        MOCK_SUPERVISOR_ERROR,
    )
    .await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "warning while a delayed retry remains",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;

    fixture
        .client
        .close_agent(&transient.instance_stream)
        .await
        .expect("close agent with pending retry");
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "closed supervised agent",
        |env| env.kind == FrameKind::AgentClosed,
    )
    .await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "orphan or fallback warning after close",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;
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

    let agent_stream = spawn_supervised_agent(&mut fixture, "supervised-error-agent", false)
        .await
        .instance_stream;

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

#[tokio::test]
async fn enabling_after_exact_codex_error_tail_emits_one_kick() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorMaxKicksPerTask { count: 1 },
    )
    .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("codex-error-tail".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: format!(
                    "recover {MOCK_USER_BUBBLES_SENTINEL} {MOCK_CODEX_INTERNAL_ERROR_TAIL}"
                ),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Codex-shaped mock agent");
    let new_agent = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "Codex-shaped NewAgent",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await
    .parse_payload::<NewAgentPayload>()
    .expect("parse NewAgent");
    let stream = new_agent.instance_stream.clone();

    for (label, predicate) in [
        ("typing active", 0_u8),
        ("normal tool request", 1),
        ("successful tool completion", 2),
        ("Codex warning", 3),
        ("typing idle", 4),
        ("recoverable error", 5),
    ] {
        let stream = stream.clone();
        wait_for_envelope(
            &mut fixture.client,
            Duration::from_secs(5),
            label,
            move |env| {
                assert_ne!(env.kind, FrameKind::AgentClosed, "tail must remain live");
                if env.kind == FrameKind::AgentError {
                    let error: protocol::AgentErrorPayload =
                        env.parse_payload().expect("parse AgentError");
                    assert!(!error.fatal, "tail error must not terminate the agent");
                }
                match (predicate, chat_event_on(env, &stream)) {
                    (0, Some(ChatEvent::TypingStatusChanged(true))) => true,
                    (1, Some(ChatEvent::ToolRequest(request))) => {
                        request.tool_name == "Bash"
                    }
                    (2, Some(ChatEvent::ToolExecutionCompleted(result))) => result.success,
                    (3, Some(ChatEvent::MessageAdded(message))) => {
                        matches!(message.sender, MessageSender::Warning)
                            && message.content == "Codex warning: Internal server error"
                    }
                    (4, Some(ChatEvent::TypingStatusChanged(false))) => true,
                    (5, Some(ChatEvent::MessageAdded(message))) => {
                        matches!(message.sender, MessageSender::Error)
                            && message.content == "Internal server error"
                    }
                    _ => false,
                }
            },
        )
        .await;
    }

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::SupervisorEnabled { enabled: true },
        })
        .await
        .expect("enable supervisor after idle error tail");
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "same-host enabled HostSettings",
        |env| {
            env.kind == FrameKind::HostSettings
                && env.parse_payload::<HostSettingsPayload>().is_ok_and(|payload| {
                    payload.settings.supervisor.enabled
                })
        },
    )
    .await;

    let kick_stream = stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "one supervisor kick after enable",
        |env| is_supervisor_kick(env, &kick_stream),
    )
    .await;
    let follow_up_stream = stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "real follow-up turn after kick",
        |env| {
            is_assistant_message_containing(
                env,
                &follow_up_stream,
                &format!("mock backend response to: {SUPERVISOR_MESSAGE_PREFIX}"),
            )
        },
    )
    .await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "second supervisor kick beyond max_kicks_per_task=1",
        |env| is_supervisor_kick(env, &stream),
    )
    .await;
}

#[tokio::test]
async fn supervisor_auto_compaction_skips_unavailable_context_at_zero_threshold() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture(0).await;

    spawn_supervised_agent(&mut fixture, "supervised-unavailable-agent", false).await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "auto-compaction when current context usage is unavailable",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn supervisor_and_auto_compact_gates_fail_independently() {
    fixture::init_tracing();

    let mut supervisor_off = Fixture::new().await;
    apply_supervisor_setting(
        &mut supervisor_off,
        HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true },
    )
    .await;
    apply_supervisor_setting(
        &mut supervisor_off,
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds: 1 },
    )
    .await;
    apply_supervisor_setting(
        &mut supervisor_off,
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 200_000 },
    )
    .await;
    spawn_supervised_agent(&mut supervisor_off, "supervisor-off-agent", true).await;
    assert_no_envelope(
        &mut supervisor_off.client,
        Duration::from_secs(5),
        "auto-compaction while the supervisor is disabled",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;

    let mut auto_compact_off = Fixture::new().await;
    apply_supervisor_setting(
        &mut auto_compact_off,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;
    apply_supervisor_setting(
        &mut auto_compact_off,
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds: 1 },
    )
    .await;
    apply_supervisor_setting(
        &mut auto_compact_off,
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 200_000 },
    )
    .await;
    spawn_supervised_agent(&mut auto_compact_off, "auto-compact-off-agent", true).await;
    assert_no_envelope(
        &mut auto_compact_off.client,
        Duration::from_secs(5),
        "auto-compaction while auto-compact is disabled",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn supervisor_auto_compaction_skips_context_below_threshold() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture(300_000).await;

    spawn_supervised_agent(&mut fixture, "supervised-below-agent", true).await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "auto-compaction below the configured context minimum",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn supervisor_auto_compaction_skips_context_equal_to_threshold() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture(250_000).await;

    spawn_supervised_agent(&mut fixture, "supervised-equal-agent", true).await;
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "auto-compaction at exactly the configured context minimum",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn supervisor_auto_compaction_runs_above_threshold_once() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture(200_000).await;

    let original = spawn_supervised_agent(&mut fixture, "supervised-done-agent", true).await;

    // 250,000 > 200,000, so a Done verdict compacts the original.
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
    let env = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "AgentClosed for the compacted original agent",
        |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|payload| payload.agent_id == original.agent_id)
        },
    )
    .await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, original.agent_id);

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

#[tokio::test]
async fn accepted_user_activity_invalidates_the_old_compaction_interval() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture_with_delay(200_000, 6).await;
    let original = spawn_supervised_agent(&mut fixture, "supervised-race-agent", true).await;

    tokio::time::sleep(Duration::from_secs(4)).await;
    fixture
        .client
        .send_message(
            &original.instance_stream,
            format!(
                "continue {MOCK_USER_BUBBLES_SENTINEL} {MOCK_CONTEXT_250K_SENTINEL} {MOCK_SUPERVISOR_DONE} {MOCK_ACTIVE_IDLE_CYCLE}"
            ),
        )
        .await
        .expect("send activity before the old expiry");
    let stream = original.instance_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "assistant response after intervening activity",
        |env| is_assistant_message_containing(env, &stream, "mock backend response to: continue"),
    )
    .await;

    assert_no_envelope(
        &mut fixture.client,
        Duration::from_secs(4),
        "replacement from the stale first inactivity interval",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "replacement after the next full inactivity interval",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn accepted_done_uses_live_auto_compact_and_threshold_settings() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorEnabled { enabled: true },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds: 1 },
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 300_000 },
    )
    .await;
    spawn_supervised_agent(&mut fixture, "live-settings-agent", true).await;
    tokio::time::sleep(Duration::from_secs(4)).await;

    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true },
    )
    .await;
    assert_no_envelope(
        &mut fixture.client,
        Duration::from_secs(1),
        "compaction while live context is below the live threshold",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 200_000 },
    )
    .await;
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "compaction after the live threshold becomes eligible",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn terminated_agent_cannot_compact_during_the_delay() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture_with_delay(200_000, 6).await;
    let agent = spawn_supervised_agent(&mut fixture, "terminated-delay-agent", true).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    fixture
        .client
        .close_agent(&agent.instance_stream)
        .await
        .expect("close agent during inactivity delay");
    assert_no_envelope(
        &mut fixture.client,
        Duration::from_secs(4),
        "auto-compaction after termination during the delay",
        |env| env.kind == FrameKind::NewAgent,
    )
    .await;
}

#[tokio::test]
async fn supervisor_awaiting_user_neither_kicks_nor_compacts() {
    fixture::init_tracing();
    let mut fixture = auto_compaction_fixture(200_000).await;
    let mut streams = Vec::new();
    for (name, waiting_case) in [
        ("awaiting-feedback", "feedback requested"),
        ("awaiting-clarification", "clarification requested"),
        ("awaiting-approval", "approval or decision requested"),
        ("awaiting-plan-review", "plan presented for review"),
    ] {
        let agent = spawn_supervised_agent_with_verdict(
            &mut fixture,
            name,
            true,
            &format!("{MOCK_SUPERVISOR_AWAITING_USER} {waiting_case}"),
        )
        .await;
        streams.push(agent.instance_stream);
    }

    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "kick or auto-compaction for an awaiting-user verdict",
        |env| {
            env.kind == FrameKind::NewAgent
                || streams.iter().any(|stream| is_supervisor_kick(env, stream))
        },
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
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds: 19 },
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 225_000 },
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
    assert_eq!(
        payload
            .settings
            .supervisor
            .auto_compact_inactivity_delay_seconds,
        19
    );
    assert_eq!(
        payload
            .settings
            .supervisor
            .auto_compact_min_context_tokens,
        225_000
    );
    assert_eq!(payload.settings.supervisor.max_kicks_per_task, 7);
    assert_eq!(payload.settings.supervisor.retry_attempts, 3);
}

#[tokio::test]
async fn invalid_supervisor_delay_returns_typed_setting_target() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    for seconds in [0_u32, 86_401] {
        fixture
            .client
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds {
                    seconds,
                },
            })
            .await
            .expect("send invalid delay setting");
        let envelope = wait_for_envelope(
            &mut fixture.client,
            Duration::from_secs(5),
            "typed invalid inactivity delay error",
            |envelope| envelope.kind == FrameKind::CommandError,
        )
        .await;
        let error: CommandErrorPayload = envelope.parse_payload().expect("parse CommandError");
        assert_eq!(
            error.setting_target,
            Some(HostSettingErrorTarget::SupervisorAutoCompactInactivityDelaySeconds)
        );
    }
}

#[tokio::test]
async fn invalid_supervisor_retry_limit_returns_typed_setting_target() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::SupervisorRetryAttempts { count: 6 },
        })
        .await
        .expect("send invalid retry setting");
    let envelope = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(5),
        "typed invalid retry-attempt error",
        |envelope| envelope.kind == FrameKind::CommandError,
    )
    .await;
    let error: CommandErrorPayload = envelope.parse_payload().expect("parse CommandError");
    assert_eq!(
        error.setting_target,
        Some(HostSettingErrorTarget::SupervisorRetryAttempts)
    );
}
