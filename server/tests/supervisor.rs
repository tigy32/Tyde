//! End-to-end coverage for supervisor verdicts, kicks, stall handling, and
//! automatic compaction.

mod fixture;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentClosedPayload, BackendKind, ChatEvent,
    CompactionMethod, CompactionTrigger, ContextCompactionCapabilityPayload,
    ContextCompactionNotifyPayload, ContextCompactionStatus, Envelope, FetchSessionHistoryPayload,
    FrameKind, ListSessionsPayload, MessageSender, NewAgentPayload,
    RequestedCompactionAvailability, RequestedCompactionRoute, SUPERVISOR_MESSAGE_PREFIX,
    SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX, SessionListPayload, SettingsWriteResultPayload,
    SpawnAgentParams, SpawnAgentPayload, StreamPath,
};
use server::backend::mock::{MockScript, MockTurn};
use settings_model::HostSettingsPayload;
use std::time::Duration;

const MOCK_SUPERVISOR_DONE: &str = "__mock_supervisor_done__";
const MOCK_SUPERVISOR_AWAITING_USER: &str = "__mock_supervisor_awaiting_user__";
const MOCK_SUPERVISOR_CONTINUE: &str = "__mock_supervisor_continue__";
const MOCK_SUPERVISOR_ERROR: &str = "__mock_supervisor_error__";

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

fn context_compaction_on(
    env: &Envelope,
    agent: &NewAgentPayload,
) -> Option<ContextCompactionNotifyPayload> {
    if env.kind != FrameKind::ContextCompactionNotify || env.stream != agent.instance_stream {
        return None;
    }
    env.parse_payload::<ContextCompactionNotifyPayload>()
        .ok()
        .filter(|payload| payload.agent_id == agent.agent_id)
}

fn is_compaction_lifecycle_on(env: &Envelope, agent: &NewAgentPayload) -> bool {
    context_compaction_on(env, agent).is_some()
        || env.kind == FrameKind::NewAgent
        || (env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == agent.agent_id))
}

async fn wait_for_native_supervisor_compaction(
    client: &mut client::Connection,
    agent: &NewAgentPayload,
    timeout: Duration,
    context: &str,
) -> ContextCompactionNotifyPayload {
    let session_id = agent
        .session_id
        .as_ref()
        .expect("supervised agent has a logical session id");
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
        assert_ne!(
            env.kind,
            FrameKind::NewAgent,
            "native supervisor compaction must not replace the live agent"
        );
        assert!(
            env.kind != FrameKind::AgentClosed
                || !env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|payload| payload.agent_id == agent.agent_id),
            "native supervisor compaction must not close the live agent"
        );
        let Some(payload) = context_compaction_on(&env, agent) else {
            continue;
        };
        if !payload.status.is_terminal() {
            continue;
        }
        assert_eq!(payload.status, ContextCompactionStatus::Completed);
        assert_eq!(&payload.logical_session_id, session_id);
        assert_eq!(payload.trigger, CompactionTrigger::SupervisorRequested);
        assert_eq!(payload.method, Some(CompactionMethod::NativeRpc));
        return payload;
    }
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
                ) =>
        {
            Some((env.stream.clone(), message.content))
        }
        _ => None,
    }
}

fn stall_interrupt_notice(env: &Envelope, stream: &StreamPath) -> Option<String> {
    match chat_event_on(env, stream)? {
        ChatEvent::MessageAdded(message)
            if matches!(message.sender, MessageSender::Warning)
                && message
                    .content
                    .starts_with(SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX) =>
        {
            Some(message.content)
        }
        _ => None,
    }
}

fn is_assistant_message_containing(env: &Envelope, stream: &StreamPath, needle: &str) -> bool {
    chat_event_on(env, stream).is_some_and(|event| assistant_message_contains(&event, needle))
}

fn assistant_message_contains(event: &ChatEvent, needle: &str) -> bool {
    match event {
        ChatEvent::MessageAdded(message) => {
            matches!(message.sender, MessageSender::Assistant { .. })
                && message.content.contains(needle)
        }
        ChatEvent::StreamEnd(data) => {
            matches!(data.message.sender, MessageSender::Assistant { .. })
                && data.message.content.contains(needle)
        }
        _ => false,
    }
}

fn native_capability_matches(
    payload: &ContextCompactionCapabilityPayload,
    agent: &NewAgentPayload,
) -> bool {
    payload.agent_id == agent.agent_id
        && matches!(
            &payload.availability,
            RequestedCompactionAvailability::Available {
                route: RequestedCompactionRoute::NativePreferred
            }
        )
}

async fn apply_supervisor_setting<V: serde::Serialize, E: serde::Serialize>(
    fixture: &mut Fixture,
    path: &str,
    value: V,
    expected: E,
) {
    fixture
        .client
        .replace_setting(path, value, expected)
        .await
        .expect("send SettingsWrite");
    fixture
        .next_frame_matching("HostSettings after supervisor SettingsWrite", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await;
}

async fn expect_supervisor_setting_rejection<V: serde::Serialize, E: serde::Serialize>(
    fixture: &mut Fixture,
    path: &str,
    value: V,
    expected: E,
) -> SettingsWriteResultPayload {
    let write_id = fixture
        .client
        .replace_setting(path, value, expected)
        .await
        .expect("send invalid SettingsWrite");
    fixture
        .next_frame_matching("rejected supervisor SettingsWrite", |env| {
            env.kind == FrameKind::SettingsWriteResult
                && env
                    .parse_payload::<SettingsWriteResultPayload>()
                    .is_ok_and(|result| result.write_id == write_id)
        })
        .await
        .parse_payload()
        .expect("parse SettingsWriteResult")
}

async fn spawn_supervised_agent(
    fixture: &mut Fixture,
    name: &str,
    report_context: bool,
) -> NewAgentPayload {
    spawn_supervised_agent_with_verdict(fixture, name, report_context, MOCK_SUPERVISOR_DONE).await
}

async fn spawn_supervised_agent_with_verdict(
    fixture: &mut Fixture,
    name: &str,
    report_context: bool,
    verdict_sentinel: &str,
) -> NewAgentPayload {
    let prompt = format!("hello {verdict_sentinel}");
    let response = format!("mock backend response to: {prompt}");
    let turn = if report_context {
        MockTurn::text_with_context_250k(response)
    } else {
        MockTurn::text(response)
    };
    let reservation = fixture
        .reserve_next_mock_launch(
            name,
            MockScript::one(turn)
                .with_user_bubbles()
                .with_unbounded_echo(),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt,
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

    let env = fixture
        .next_frame_matching("NewAgent", |env| env.kind == FrameKind::NewAgent)
        .await;
    let mut new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let mut saw_native_capability = false;
    let mut saw_initial_response = false;
    let mut logical_session_id = None;
    fixture
        .next_frame_matching(
            "initial mock turn and native compaction capability",
            |env| {
                if env.stream != agent_stream {
                    return false;
                }
                match env.kind {
                    FrameKind::AgentBootstrap => {
                        let bootstrap: AgentBootstrapPayload =
                            env.parse_payload().expect("parse AgentBootstrap");
                        for event in bootstrap.events {
                            match event {
                                AgentBootstrapEvent::ContextCompactionCapability(payload) => {
                                    if native_capability_matches(&payload, &new_agent) {
                                        saw_native_capability = true;
                                        logical_session_id = Some(payload.logical_session_id);
                                    }
                                }
                                AgentBootstrapEvent::ChatEvent(event) => {
                                    saw_initial_response |= assistant_message_contains(
                                        &event,
                                        "mock backend response to: hello",
                                    );
                                }
                                AgentBootstrapEvent::AgentStart(_)
                                | AgentBootstrapEvent::AgentError(_)
                                | AgentBootstrapEvent::SessionSettings(_)
                                | AgentBootstrapEvent::QueuedMessages(_)
                                | AgentBootstrapEvent::AgentActivityStats(_)
                                | AgentBootstrapEvent::ContextCompaction(_)
                                | AgentBootstrapEvent::HasPriorHistory { .. } => {}
                            }
                        }
                    }
                    FrameKind::ContextCompactionCapability => {
                        let payload: ContextCompactionCapabilityPayload = env
                            .parse_payload()
                            .expect("parse ContextCompactionCapability");
                        if native_capability_matches(&payload, &new_agent) {
                            saw_native_capability = true;
                            logical_session_id = Some(payload.logical_session_id);
                        }
                    }
                    FrameKind::ChatEvent => {
                        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                        saw_initial_response |=
                            assistant_message_contains(&event, "mock backend response to: hello");
                    }
                    _ => {}
                }
                saw_native_capability && saw_initial_response
            },
        )
        .await;
    drop(reservation);
    new_agent.session_id = logical_session_id;
    new_agent
}

async fn auto_compaction_fixture(threshold: u64) -> Fixture {
    auto_compaction_fixture_with_delay(threshold, 1).await
}

async fn auto_compaction_fixture_with_delay(threshold: u64, delay_seconds: u32) -> Fixture {
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_on_success",
        true,
        false,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_inactivity_delay_seconds",
        delay_seconds,
        300_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_min_context_tokens",
        threshold,
        200_000_u64,
    )
    .await;
    fixture
}

#[tokio::test]
async fn exhausted_supervisor_failure_warns_once_per_activity_generation() {
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(&mut fixture, "/supervisor/retry_attempts", 0, 1_u32).await;
    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;

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
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
                before_seq: None,
                limit: 100,
            },
        )
        .await
        .expect("fetch affected actor history");
    let history = fixture
        .next_frame_matching("affected actor history", |env| {
            env.kind == FrameKind::SessionHistory && env.stream == affected.instance_stream
        })
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
    fixture
        .next_frame_matching("new generation assistant turn", |env| {
            is_assistant_message_containing(env, &affected_stream, "new generation")
        })
        .await;
    let second = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "new generation supervisor failure warning",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;
    let (second_stream, second_copy) =
        supervisor_failure_warning(&second).expect("second supervisor failure warning");
    assert_eq!(second_stream, affected.instance_stream);
    assert_eq!(second_copy, singular);
    fixture
        .client
        .fetch_session_history(
            &affected.instance_stream,
            FetchSessionHistoryPayload {
                agent_id: affected.agent_id.clone(),
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
                before_seq: None,
                limit: 100,
            },
        )
        .await
        .expect("fetch both warning generations");
    let history = fixture
        .next_frame_matching("history for both warning generations", |env| {
            env.kind == FrameKind::SessionHistory && env.stream == affected.instance_stream
        })
        .await
        .parse_payload::<protocol::SessionHistoryPayload>()
        .expect("parse history for both warning generations");
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
        2
    );
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "duplicate warning for the second unchanged generation",
        |env| supervisor_failure_warning(env).is_some(),
    )
    .await;
}

#[tokio::test]
async fn transient_supervisor_failure_and_closed_agent_remain_silent() {
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(&mut fixture, "/supervisor/retry_attempts", 1, 1_u32).await;
    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;
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
    fixture
        .next_frame_matching("closed supervised agent", |env| {
            env.kind == FrameKind::AgentClosed
        })
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
    let mut fixture = Fixture::new().await;

    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;
    apply_supervisor_setting(&mut fixture, "/supervisor/max_kicks_per_task", 1, 3_u32).await;

    let agent = spawn_supervised_agent(&mut fixture, "supervised-error-agent", false).await;
    let agent_stream = agent.instance_stream.clone();
    fixture
        .mock_by_id(&agent.agent_id)
        .await
        .enqueue(MockTurn::error_card(
            "mock backend emitted error without idle",
        ))
        .await;
    fixture
        .client
        .send_message(&agent_stream, "trigger backend error".to_owned())
        .await
        .expect("send backend error trigger failed");

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
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(&mut fixture, "/supervisor/max_kicks_per_task", 1, 3_u32).await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "codex-error-tail",
            MockScript::one(MockTurn::codex_internal_error_tail())
                .with_user_bubbles()
                .with_unbounded_echo(),
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
                prompt: "recover".to_owned(),
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
    let new_agent = fixture
        .next_frame_matching("Codex-shaped NewAgent", |env| {
            env.kind == FrameKind::NewAgent
        })
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
        fixture
            .next_frame_matching(label, move |env| {
                assert_ne!(env.kind, FrameKind::AgentClosed, "tail must remain live");
                if env.kind == FrameKind::AgentError {
                    let error: protocol::AgentErrorPayload =
                        env.parse_payload().expect("parse AgentError");
                    assert!(!error.fatal, "tail error must not terminate the agent");
                }
                match (predicate, chat_event_on(env, &stream)) {
                    (0, Some(ChatEvent::TypingStatusChanged(true))) => true,
                    (1, Some(ChatEvent::ToolRequest(request))) => {
                        fixture::tool_request_name(&request) == "Bash"
                    }
                    (2, Some(ChatEvent::ToolExecutionCompleted(result))) => {
                        fixture::tool_completion_succeeded(&result)
                    }
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
            })
            .await;
    }
    drop(reservation);

    fixture
        .client
        .replace_setting("/supervisor/enabled", true, false)
        .await
        .expect("enable supervisor after idle error tail");
    fixture
        .next_frame_matching("same-host enabled HostSettings", |env| {
            env.kind == FrameKind::HostSettings
                && env
                    .parse_payload::<HostSettingsPayload>()
                    .is_ok_and(|payload| payload.settings.supervisor.enabled)
        })
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
    let mut supervisor_off = Fixture::new().await;
    apply_supervisor_setting(
        &mut supervisor_off,
        "/supervisor/auto_compact_on_success",
        true,
        false,
    )
    .await;
    apply_supervisor_setting(
        &mut supervisor_off,
        "/supervisor/auto_compact_inactivity_delay_seconds",
        1,
        300_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut supervisor_off,
        "/supervisor/auto_compact_min_context_tokens",
        200_000,
        200_000_u64,
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
    apply_supervisor_setting(&mut auto_compact_off, "/supervisor/enabled", true, false).await;
    apply_supervisor_setting(
        &mut auto_compact_off,
        "/supervisor/auto_compact_inactivity_delay_seconds",
        1,
        300_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut auto_compact_off,
        "/supervisor/auto_compact_min_context_tokens",
        200_000,
        200_000_u64,
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
    let mut fixture = auto_compaction_fixture(200_000).await;

    let original = spawn_supervised_agent(&mut fixture, "supervised-done-agent", true).await;

    // The failing run reached the eligible 250,000 > 200,000 threshold but
    // timed out only on the stale replacement NewAgent. The mock advertises
    // native JSON RPC, so the same-session typed terminal is completion.
    wait_for_native_supervisor_compaction(
        &mut fixture.client,
        &original,
        SUPERVISION_WAIT,
        "terminal native supervisor auto-compaction",
    )
    .await;

    // The in-place operation becomes dormant after completion. No further
    // compaction lifecycle or destructive legacy replacement may start.
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "second auto-compaction of the live agent",
        |env| is_compaction_lifecycle_on(env, &original),
    )
    .await;
}

#[tokio::test]
async fn accepted_user_activity_invalidates_the_old_compaction_interval() {
    let mut fixture = auto_compaction_fixture_with_delay(200_000, 6).await;
    let original = spawn_supervised_agent(&mut fixture, "supervised-race-agent", true).await;

    tokio::time::sleep(Duration::from_secs(4)).await;
    fixture
        .mock_by_id(&original.agent_id)
        .await
        .enqueue(
            MockTurn::text_with_context_250k(format!(
                "mock backend response to: continue {MOCK_SUPERVISOR_DONE}"
            ))
            .with_active_idle_cycle(),
        )
        .await;
    fixture
        .client
        .send_message(
            &original.instance_stream,
            format!("continue {MOCK_SUPERVISOR_DONE}"),
        )
        .await
        .expect("send activity before the old expiry");
    let stream = original.instance_stream.clone();
    fixture
        .next_frame_matching("assistant response after intervening activity", |env| {
            is_assistant_message_containing(env, &stream, "mock backend response to: continue")
        })
        .await;

    assert_no_envelope(
        &mut fixture.client,
        Duration::from_secs(4),
        "compaction from the stale first inactivity interval",
        |env| is_compaction_lifecycle_on(env, &original),
    )
    .await;
    // The failing run preserved this quiet window and timed out only when the
    // next eligible interval still required a replacement NewAgent.
    wait_for_native_supervisor_compaction(
        &mut fixture.client,
        &original,
        SUPERVISION_WAIT,
        "native compaction after the next full inactivity interval",
    )
    .await;
}

#[tokio::test]
async fn accepted_done_uses_live_auto_compact_and_threshold_settings() {
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_inactivity_delay_seconds",
        1,
        300_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_min_context_tokens",
        300_000,
        200_000_u64,
    )
    .await;
    let agent = spawn_supervised_agent(&mut fixture, "live-settings-agent", true).await;
    tokio::time::sleep(Duration::from_secs(4)).await;

    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_on_success",
        true,
        false,
    )
    .await;
    assert_no_envelope(
        &mut fixture.client,
        Duration::from_secs(1),
        "compaction while live context is below the live threshold",
        |env| is_compaction_lifecycle_on(env, &agent),
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_min_context_tokens",
        200_000,
        300_000_u64,
    )
    .await;
    // The failing run honored the live 300,000-token threshold and timed out
    // only because the newly eligible native operation was expected to spawn.
    wait_for_native_supervisor_compaction(
        &mut fixture.client,
        &agent,
        SUPERVISION_WAIT,
        "native compaction after the live threshold becomes eligible",
    )
    .await;
}

#[tokio::test]
async fn terminated_agent_cannot_compact_during_the_delay() {
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

#[tokio::test]
async fn supervisor_settings_round_trip_over_the_wire() {
    let mut fixture = Fixture::new().await;

    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_on_success",
        true,
        false,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_inactivity_delay_seconds",
        19_u32,
        300_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/auto_compact_min_context_tokens",
        225_000_u64,
        200_000_u64,
    )
    .await;
    apply_supervisor_setting(&mut fixture, "/supervisor/max_kicks_per_task", 7_u32, 3_u32).await;
    apply_supervisor_setting(&mut fixture, "/supervisor/retry_attempts", 2_u32, 1_u32).await;

    fixture
        .client
        .replace_setting("/supervisor/retry_attempts", 3_u32, 2_u32)
        .await
        .expect("send final SettingsWrite");
    let env = fixture
        .next_frame_matching("final HostSettings fan-out", |env| {
            env.kind == FrameKind::HostSettings
                && env
                    .parse_payload::<HostSettingsPayload>()
                    .is_ok_and(|payload| payload.settings.supervisor.retry_attempts == 3)
        })
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
        payload.settings.supervisor.auto_compact_min_context_tokens,
        225_000
    );
    assert_eq!(payload.settings.supervisor.max_kicks_per_task, 7);
    assert_eq!(payload.settings.supervisor.retry_attempts, 3);
}

#[tokio::test]
async fn invalid_supervisor_delay_returns_pointer_error() {
    let mut fixture = Fixture::new().await;
    for seconds in [0_u32, 86_401] {
        let result = expect_supervisor_setting_rejection(
            &mut fixture,
            "/supervisor/auto_compact_inactivity_delay_seconds",
            seconds,
            300_u32,
        )
        .await;
        assert!(!result.applied);
        assert_eq!(
            result.field_errors[0].pointer,
            "/supervisor/auto_compact_inactivity_delay_seconds"
        );
    }
}

#[tokio::test]
async fn invalid_supervisor_retry_limit_returns_pointer_error() {
    let mut fixture = Fixture::new().await;
    let result = expect_supervisor_setting_rejection(
        &mut fixture,
        "/supervisor/retry_attempts",
        6_u32,
        1_u32,
    )
    .await;
    assert!(!result.applied);
    assert_eq!(result.field_errors[0].pointer, "/supervisor/retry_attempts");
}

/// Reopening a saved session replays its transcript, which reaches the
/// supervisor looking exactly like an agent that just finished a turn. The
/// default must leave that history alone; enabling the opt-in must judge the
/// very agents that were waiting on it.
#[tokio::test]
async fn restored_session_is_supervised_only_after_the_restore_opt_in() {
    let mut fixture = Fixture::new().await;
    let source_name = "restore-supervision-source";
    let prompt = format!("hello {MOCK_SUPERVISOR_CONTINUE}");
    let reservation = fixture
        .reserve_next_mock_launch(
            source_name,
            MockScript::one(MockTurn::text(format!(
                "mock backend response to: {prompt}"
            )))
            .with_user_bubbles()
            .with_unbounded_echo(),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(source_name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: prompt.clone(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn restore source");
    let source: NewAgentPayload = fixture
        .next_frame_matching("restore source NewAgent", |env| {
            env.kind == FrameKind::NewAgent
        })
        .await
        .parse_payload()
        .expect("parse restore source NewAgent");
    let source_stream = source.instance_stream.clone();
    fixture
        .next_frame_matching("restore source turn", |env| {
            is_assistant_message_containing(env, &source_stream, "mock backend response to:")
        })
        .await;
    drop(reservation);

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions for restore");
    let sessions: SessionListPayload = fixture
        .next_frame_matching("restore SessionList", |env| {
            env.kind == FrameKind::SessionList
        })
        .await
        .parse_payload()
        .expect("parse restore SessionList");
    let session_id = sessions
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some(source_name))
        .expect("restore source session is listed")
        .id
        .clone();

    fixture
        .client
        .close_agent(&source.instance_stream)
        .await
        .expect("close restore source");
    fixture
        .next_frame_matching("restore source AgentClosed", |env| {
            env.kind == FrameKind::AgentClosed
        })
        .await;

    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "restored-agent",
            MockScript::new().with_user_bubbles().with_unbounded_echo(),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("restored-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume the saved session");
    let restored: NewAgentPayload = fixture
        .next_frame_matching("restored NewAgent", |env| env.kind == FrameKind::NewAgent)
        .await
        .parse_payload()
        .expect("parse restored NewAgent");
    let restored_stream = restored.instance_stream.clone();
    // Replayed history is recorded without being broadcast as live events; the
    // client is attached once the replay has settled, so its bootstrap is what
    // proves the restored transcript — including the replayed request the
    // supervisor would read — is in place.
    let bootstrap: protocol::AgentBootstrapPayload = wait_for_envelope(
        &mut fixture.client,
        Duration::from_secs(10),
        "restored agent bootstrap after replay",
        |env| env.kind == FrameKind::AgentBootstrap && env.stream == restored_stream,
    )
    .await
    .parse_payload()
    .expect("parse restored AgentBootstrap");
    drop(reservation);
    assert!(
        bootstrap.events.iter().any(|event| matches!(
            event,
            protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                if matches!(message.sender, MessageSender::User)
                    && message.content == prompt
        )),
        "the restored transcript must replay the original request the supervisor reads"
    );

    let quiet_stream = restored_stream.clone();
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "supervisor kick on a restored agent before the opt-in",
        |env| is_supervisor_kick(env, &quiet_stream),
    )
    .await;

    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/supervise_restored_agents",
        true,
        false,
    )
    .await;
    let kick = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "supervisor kick after enabling restored-agent supervision",
        |env| is_supervisor_kick(env, &restored_stream),
    )
    .await;
    assert_eq!(kick.stream, restored.instance_stream);
}

/// A turn that stops producing anything is cancelled, the cancel is attributed
/// to the supervisor in the transcript, and the truncated turn is then judged —
/// a user cancel would have suppressed supervision instead.
#[tokio::test]
async fn stalled_turn_is_interrupted_then_judged_once() {
    let mut fixture = Fixture::new().await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/stall_timeout_seconds",
        1,
        1_800_u32,
    )
    .await;
    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/stall_timeout_enabled",
        true,
        false,
    )
    .await;
    apply_supervisor_setting(&mut fixture, "/supervisor/enabled", true, false).await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "stalled-agent",
            MockScript::one(MockTurn::held_text(
                "mock backend held response to: stall forever",
            ))
            .with_user_bubbles()
            .with_unbounded_echo(),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("stalled-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "stall forever".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn a stalling agent");
    let agent: NewAgentPayload = fixture
        .next_frame_matching("stalled NewAgent", |env| env.kind == FrameKind::NewAgent)
        .await
        .parse_payload()
        .expect("parse stalled NewAgent");
    let agent_stream = agent.instance_stream.clone();

    let notice = wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "stall interrupt notice",
        |env| stall_interrupt_notice(env, &agent_stream).is_some(),
    )
    .await;
    drop(reservation);
    let notice_copy =
        stall_interrupt_notice(&notice, &agent_stream).expect("stall interrupt notice payload");
    assert!(
        notice_copy.contains("1 second"),
        "the notice must name the configured window: {notice_copy}"
    );
    assert!(
        notice_copy.contains("no progress") && notice_copy.contains("supervisor"),
        "the notice must say why the turn stopped and who stopped it: {notice_copy}"
    );

    let kick_stream = agent_stream.clone();
    wait_for_envelope(
        &mut fixture.client,
        SUPERVISION_WAIT,
        "supervisor follow-up after the stalled turn was cancelled",
        |env| is_supervisor_kick(env, &kick_stream),
    )
    .await;

    // The follow-up turn is ordinary work, so the truncation must not keep
    // re-arming: no second interrupt and no second follow-up.
    let quiet_stream = agent_stream.clone();
    assert_no_envelope(
        &mut fixture.client,
        QUIET_WAIT,
        "repeat stall interrupt or follow-up after the agent resumed working",
        |env| {
            stall_interrupt_notice(env, &quiet_stream).is_some()
                || is_supervisor_kick(env, &quiet_stream)
        },
    )
    .await;
}

#[tokio::test]
async fn invalid_supervisor_stall_timeout_returns_pointer_error() {
    let mut fixture = Fixture::new().await;
    for seconds in [0_u32, 86_401] {
        let result = expect_supervisor_setting_rejection(
            &mut fixture,
            "/supervisor/stall_timeout_seconds",
            seconds,
            1_800_u32,
        )
        .await;
        assert!(!result.applied);
        assert_eq!(
            result.field_errors[0].pointer,
            "/supervisor/stall_timeout_seconds"
        );
    }

    apply_supervisor_setting(
        &mut fixture,
        "/supervisor/stall_timeout_seconds",
        900,
        1_800_u32,
    )
    .await;
    fixture
        .client
        .replace_setting("/supervisor/supervise_restored_agents", true, false)
        .await
        .expect("send restore opt-in");
    let payload: HostSettingsPayload = fixture
        .next_frame_matching("HostSettings after the restore opt-in", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await
        .parse_payload()
        .expect("parse HostSettings");
    assert_eq!(payload.settings.supervisor.stall_timeout_seconds, 900);
    assert!(payload.settings.supervisor.supervise_restored_agents);
    assert!(
        !payload.settings.supervisor.stall_timeout_enabled,
        "the window and the opt-in must not switch interrupting on by themselves"
    );
}
