mod fixture;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorPayload, AgentStartPayload,
    BackendAccessMode, BackendKind, ChatEvent, DeleteSessionPayload, Envelope,
    FetchSessionHistoryPayload, FrameKind, ListSessionsPayload, NewAgentPayload, Project,
    ProjectCreatePayload, ProjectNotifyPayload, ProjectRootPath, SessionHistoryPayload, SessionId,
    SessionListPayload, SessionSettingValue, SessionSettingsValues, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};
use server::backend::mock::{MockScript, MockTurn};
use server::store::session::SessionStore;
use std::path::Path;
use std::time::Duration;

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = fixture::next_logical_frame_on(client, context).await;
        eprintln!(
            "TYDE SESSION RESUME WAIT context={context} kind={} seq={}",
            env.kind, env.seq
        );
        if fixture::is_routine_control_plane_frame(&env)
            || matches!(
                env.kind,
                FrameKind::HostSettings
                    | FrameKind::BackendCapacity
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::SessionList
                    | FrameKind::SessionSummaryCountUpdated
                    | FrameKind::TaskTokenUsage
                    | FrameKind::WorkflowNotify
                    | FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::AgentActivityStats
                    | FrameKind::ContextCompactionNotify
                    | FrameKind::ContextCompactionCapability
            )
        {
            continue;
        }
        return env;
    }
}

async fn expect_raw_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    fixture::next_frame_matching_on(client, context, |env| {
        if fixture::is_routine_control_plane_frame(env) {
            return false;
        }
        if env.stream != *stream {
            return false;
        }
        if env.kind == kind {
            return true;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::ContextCompactionNotify
                | FrameKind::ContextCompactionCapability
        ) {
            return false;
        }
        panic!(
            "wait for {kind} on {stream} during {context} received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    })
    .await
}

async fn expect_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> ChatEvent {
    loop {
        let env = expect_next_event(client, context).await;
        if env.stream != *stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ChatEvent);
        return env.parse_payload().expect("failed to parse ChatEvent");
    }
}

async fn expect_chat_event(client: &mut client::Connection, context: &str) -> ChatEvent {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::ChatEvent {
            continue;
        }
        return env.parse_payload().expect("failed to parse ChatEvent");
    }
}

async fn expect_agent_start_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.stream != *stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::AgentStart);
        return env.parse_payload().expect("failed to parse AgentStart");
    }
}

async fn expect_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let event =
        expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true) or StreamStart")
            .await;
    let delta = match event {
        ChatEvent::TypingStatusChanged(true) => {
            let event = expect_chat_event_on_stream(client, stream, "StreamStart").await;
            match event {
                ChatEvent::StreamStart(_) => {
                    expect_chat_event_on_stream(client, stream, "StreamDelta").await
                }
                delta @ ChatEvent::StreamDelta(_) => delta,
                other => panic!("expected StreamStart or StreamDelta, got {other:?}"),
            }
        }
        ChatEvent::StreamStart(_) => {
            expect_chat_event_on_stream(client, stream, "StreamDelta").await
        }
        delta @ ChatEvent::StreamDelta(_) => delta,
        other => panic!("expected TypingStatusChanged(true) or StreamStart, got {other:?}"),
    };
    match &delta {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let event = expect_chat_event_on_stream(client, stream, "StreamEnd").await;
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let event = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

fn assert_bootstrap_prior_history_indicator(
    payload: &AgentBootstrapPayload,
    expected_message_count: u32,
) -> u64 {
    let before_seq = payload.events.iter().find_map(|event| match event {
        AgentBootstrapEvent::HasPriorHistory {
            message_count,
            before_seq,
        } if *message_count == expected_message_count => Some(*before_seq),
        _ => None,
    });
    assert!(
        before_seq.is_some(),
        "AgentBootstrap should include HasPriorHistory({expected_message_count}), got {:?}",
        payload.events
    );
    before_seq.expect("checked above")
}

fn assert_bootstrap_has_no_prior_history_indicator(payload: &AgentBootstrapPayload) {
    assert!(
        payload
            .events
            .iter()
            .all(|event| !matches!(event, AgentBootstrapEvent::HasPriorHistory { .. })),
        "AgentBootstrap should not include HasPriorHistory: {:?}",
        payload.events
    );
}

fn assert_bootstrap_tail_messages(
    payload: &AgentBootstrapPayload,
    expected_chronological: &[&str],
) {
    let contents = payload
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) => {
                Some(message.content.as_str())
            }
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => {
                Some(end.message.content.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        contents.len(),
        expected_chronological.len(),
        "unexpected bootstrap tail messages: {:?}",
        payload.events
    );
    for (content, expected) in contents.iter().zip(expected_chronological) {
        assert!(
            content.contains(expected),
            "bootstrap message content {content:?} did not contain {expected:?}",
        );
    }
}

/// Every message body a bootstrap carries, in the order the user would read
/// them.
fn bootstrap_message_contents(payload: &AgentBootstrapPayload) -> Vec<String> {
    payload
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) => {
                Some(message.content.clone())
            }
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => {
                Some(end.message.content.clone())
            }
            _ => None,
        })
        .collect()
}

fn bootstrap_agent_start(payload: &AgentBootstrapPayload) -> &AgentStartPayload {
    payload
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentStart(start) => Some(start),
            _ => None,
        })
        .expect("AgentBootstrap should include AgentStart")
}

async fn fetch_history_page(
    client: &mut client::Connection,
    stream: &StreamPath,
    agent_id: protocol::AgentId,
    before_seq: Option<u64>,
    limit: u32,
) -> SessionHistoryPayload {
    client
        .fetch_session_history(
            stream,
            FetchSessionHistoryPayload {
                agent_id,
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
                before_seq,
                limit,
            },
        )
        .await
        .expect("fetch_session_history failed");

    let env =
        expect_raw_event_on_stream(client, stream, FrameKind::SessionHistory, "SessionHistory")
            .await;
    env.parse_payload()
        .expect("failed to parse SessionHistoryPayload")
}

// Pages carry events in the order they happened, exactly like the live
// stream: both clients replay a page front to back before prepending it, so a
// reversed page renders each loaded window upside down and attaches tool
// cards to the wrong turns.
fn assert_history_page(
    page: &SessionHistoryPayload,
    expected_oldest_first: &[&str],
    expected_has_more_before: bool,
) {
    assert_eq!(
        page.events.len(),
        expected_oldest_first.len() * 3,
        "unexpected SessionHistory event count: {:?}",
        page.events
    );
    for (response, expected) in page
        .events
        .as_chunks::<3>()
        .0
        .iter()
        .zip(expected_oldest_first)
    {
        let [
            ChatEvent::StreamStart(_),
            ChatEvent::StreamDelta(delta),
            ChatEvent::StreamEnd(end),
        ] = response
        else {
            panic!("expected chronological response boundary, got {response:?}");
        };
        assert!(
            end.message.content.contains(expected),
            "history message content {:?} did not contain {expected:?}",
            end.message.content
        );
        assert!(
            delta.text.contains(expected),
            "history delta text {:?} did not contain {expected:?}",
            delta.text
        );
    }
    assert_eq!(page.has_more_before, expected_has_more_before);
}

async fn wait_for_session_list(
    client: &mut client::Connection,
    context: &str,
) -> SessionListPayload {
    let env = fixture::next_frame_matching_on(client, context, |env| {
        if fixture::is_routine_control_plane_frame(env) {
            return false;
        }
        if env.kind == FrameKind::AgentBootstrap {
            return false;
        }
        // A live capability update can arrive after deletion, before SessionList.
        // It does not change whether the deleted session remains in that list.
        if env.kind == FrameKind::ContextCompactionCapability {
            return false;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::NewAgent
                | FrameKind::AgentStart
                | FrameKind::AgentError
                | FrameKind::SessionSummaryCountUpdated
                | FrameKind::TaskTokenUsage
                | FrameKind::ChatEvent
        ) {
            return false;
        }
        if env.kind == FrameKind::SessionList {
            return true;
        }
        panic!(
            "wait_for_session_list({context}) received unexpected event: kind={} stream={}",
            env.kind, env.stream
        );
    })
    .await;
    env.parse_payload()
        .expect("failed to parse SessionListPayload")
}

async fn expect_turn(client: &mut client::Connection, expected_text: &str) {
    let event = expect_chat_event(client, "TypingStatusChanged(true) or StreamStart").await;
    let delta = match event {
        ChatEvent::TypingStatusChanged(true) => {
            let event = expect_chat_event(client, "StreamStart").await;
            match event {
                ChatEvent::StreamStart(_) => expect_chat_event(client, "StreamDelta").await,
                delta @ ChatEvent::StreamDelta(_) => delta,
                other => panic!("expected StreamStart or StreamDelta, got {other:?}"),
            }
        }
        ChatEvent::StreamStart(_) => expect_chat_event(client, "StreamDelta").await,
        delta @ ChatEvent::StreamDelta(_) => delta,
        other => panic!("expected TypingStatusChanged(true) or StreamStart, got {other:?}"),
    };

    match &delta {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let event = expect_chat_event(client, "StreamEnd").await;
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let event = expect_chat_event(client, "TypingStatusChanged(false)").await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    fixture::assert_no_interesting_frame_on(client, duration, context, |env| {
        matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::BackendCapacity
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
        )
    })
    .await;
}

async fn expect_no_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    duration: Duration,
    context: &str,
) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env))) if fixture::is_builtin_team_custom_agent_notify(&env) => continue,
            Ok(Ok(Some(env))) if env.stream == *stream && env.kind == FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse unexpected ChatEvent");
                panic!("unexpected live ChatEvent on {stream} before {context}: {event:?}");
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    env.parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
}

#[tokio::test]
async fn list_sessions_and_resume_agent() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumable".to_owned()),
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
        .await
        .expect("spawn resumable agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _: NewAgentPayload = env.parse_payload().expect("parse NewAgent");

    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    // A spawn also sends an unsolicited list (observed with message_count=0)
    // that may remain queued after StreamEnd on the separate agent stream.
    // Assert on a requested current snapshot, not that older notification.
    let mut list_client = fixture.connect().await;
    list_client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");

    let list = wait_for_session_list(&mut list_client, "SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one stored session");
    let session = &list.sessions[0];
    assert_eq!(session.backend_kind, BackendKind::Claude);
    assert_eq!(session.workspace_roots, vec!["/tmp/test".to_owned()]);
    assert!(session.resumable);
    assert_eq!(session.message_count, 1);

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");

    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");
    let start = bootstrap_agent_start(&payload);
    assert_eq!(start.agent_id, resumed.agent_id);
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    let mut list_client = fixture.connect().await;
    list_client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after resume failed");

    let list = wait_for_session_list(&mut list_client, "SessionList after resume").await;
    assert_eq!(
        list.sessions.len(),
        1,
        "resume should reuse the same session"
    );
    assert_eq!(list.sessions[0].id, session.id);
    assert_eq!(list.sessions[0].message_count, 2);
}

fn insert_foreign_session(path: &Path, foreign_id: &str) {
    // The contract is another writer's committed session surviving a live turn,
    // not the legacy JSON representation. SQLite is now authoritative.
    let store = SessionStore::load(path.to_owned()).expect("open independent session writer");
    store
        .upsert_backend_session(
            &server::backend::BackendSession {
                id: SessionId(foreign_id.to_owned()),
                backend_kind: BackendKind::Claude,
                workspace_roots: vec!["/tmp/foreign".to_owned()],
                title: None,
                token_count: None,
                created_at_ms: Some(1),
                updated_at_ms: Some(1),
                resumable: true,
            },
            None,
            None,
            None,
            None,
        )
        .expect("persist independent session");
}

#[tokio::test]
async fn session_store_keeps_foreign_records_across_turn() {
    let mut fixture = Fixture::new_with_store_files(
        r#"{"records": {}, "write_seq": 41}"#,
        r#"{"version": 2, "records": {}, "write_seq": 17}"#,
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("live-session".to_owned()),
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
        .await
        .expect("spawn live session failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");

    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, "SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one stored session");
    let session_id = list.sessions[0].id.clone();
    assert_eq!(list.sessions[0].message_count, 1);

    let sessions_path = fixture.store_dir().join("sessions.json");
    insert_foreign_session(&sessions_path, "foreign-session");

    fixture
        .client
        .send_message(&new_agent.instance_stream, "follow-up".to_owned())
        .await
        .expect("send follow-up failed");
    expect_turn(&mut fixture.client, "mock backend response to: follow-up").await;

    let store = SessionStore::load(sessions_path).expect("load session store");
    let records = store.list().expect("list session records");
    assert_eq!(records.len(), 2, "live write must keep the foreign record");
    let live = records
        .iter()
        .find(|record| record.id == session_id)
        .expect("live session record");
    assert_eq!(live.message_count, 2);
    assert!(
        records
            .iter()
            .any(|record| record.id.0 == "foreign-session"),
        "foreign session record missing after live turn: {:?}",
        records
            .iter()
            .map(|record| record.id.0.as_str())
            .collect::<Vec<_>>()
    );
}

/// Collects what a restart replayed for `sessions`: the reconstructed
/// `NewAgent` for each, plus the `AgentBootstrap` that follows on its instance
/// stream. One scan for all of it — the frame readers discard what they skip,
/// so searching per session or per frame kind throws away the frame the next
/// search is waiting for.
async fn collect_restart_replay(
    fixture: &mut Fixture,
    bootstrap: &settings_model::HostBootstrapPayload,
    sessions: &[SessionId],
) -> (
    std::collections::HashMap<SessionId, NewAgentPayload>,
    std::collections::HashMap<StreamPath, AgentBootstrapPayload>,
) {
    let mut agents = std::collections::HashMap::new();
    let mut bootstraps = std::collections::HashMap::<StreamPath, AgentBootstrapPayload>::new();
    for agent in &bootstrap.agents {
        if let Some(session_id) = agent.session_id.as_ref()
            && sessions.contains(session_id)
        {
            agents.insert(session_id.clone(), agent.clone());
        }
    }
    loop {
        let have_all = agents.len() == sessions.len()
            && agents
                .values()
                .all(|agent| bootstraps.contains_key(&agent.instance_stream));
        if have_all {
            return (agents, bootstraps);
        }
        let env = fixture::next_frame_matching_on(&mut fixture.client, "restart replay", |env| {
            matches!(env.kind, FrameKind::NewAgent | FrameKind::AgentBootstrap)
        })
        .await;
        match env.kind {
            FrameKind::NewAgent => {
                let agent: NewAgentPayload = env.parse_payload().expect("parse restored NewAgent");
                if let Some(session_id) = agent.session_id.as_ref()
                    && sessions.contains(session_id)
                {
                    agents.insert(session_id.clone(), agent);
                }
            }
            FrameKind::AgentBootstrap => {
                let payload: AgentBootstrapPayload =
                    env.parse_payload().expect("parse restored AgentBootstrap");
                bootstraps.insert(env.stream.clone(), payload);
            }
            kind => unreachable!("unexpected restart replay frame {kind:?}"),
        }
    }
}

fn stored_access_mode(fixture: &Fixture, session_id: &SessionId) -> BackendAccessMode {
    let store =
        SessionStore::load(fixture.store_dir().join("sessions.json")).expect("load session store");
    store
        .get(session_id)
        .expect("stored session record")
        .access_mode
}

/// A read-only agent that is open when Tyde restarts has to come back
/// read-only. Restoration reopens it through the resume path, which resolves a
/// fresh user configuration; if that path does not reapply the stored mode the
/// agent returns unrestricted and startup persists that default over the saved
/// one, so the restriction is gone from disk too and every later resume is
/// unrestricted as well.
/// The desktop builds its host in Tauri's setup hook, outside any tokio
/// runtime, so the restoration pass runs on a runtime it builds itself.
/// Restored agents spawn their actor tasks onto that runtime; if it is dropped
/// when the pass finishes, every card it just rebuilt is backed by a dead
/// actor and the user's agents answer nothing. Constructing the replacement
/// host on a plain OS thread is what puts restoration on that path.
/// Restoration and a client resume both resolve discovery and a spawn
/// configuration before they register an actor, so the ownership check and the
/// registration are separated by awaits. Without a claim held across that gap,
/// a user picking the session out of Sessions while the restart is still
/// restoring gives one session two owners: two cards, two backends, and a
/// close of either one clearing the marker while the other is still open.
/// Resuming a session that is already open deliberately produces a second
/// card, so one session can back several. Closing one of them withdraws that
/// card, not the session's restoration intent: nothing re-marks a session
/// during ordinary turns, so clearing the marker on the first close would
/// leave the cards still open unable to come back after a restart.
#[tokio::test]
async fn closing_one_card_does_not_strand_another_on_the_same_session() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("shared session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/shared-session".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn first card");
    let first: NewAgentPayload = expect_next_event(&mut fixture.client, "first NewAgent")
        .await
        .parse_payload()
        .expect("parse first NewAgent");
    let start =
        expect_agent_start_on_stream(&mut fixture.client, &first.instance_stream, "first start")
            .await;
    let session = start.session_id.expect("shared session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &first.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the same session as a second card");
    let second: NewAgentPayload = expect_next_event(&mut fixture.client, "second NewAgent")
        .await
        .parse_payload()
        .expect("parse second NewAgent");
    expect_agent_start_on_stream(&mut fixture.client, &second.instance_stream, "second start")
        .await;
    assert_ne!(
        first.agent_id, second.agent_id,
        "resuming an open session must produce a distinct card"
    );

    fixture
        .client
        .close_agent(&second.instance_stream)
        .await
        .expect("close the second card");
    fixture::next_frame_matching_on(&mut fixture.client, "second AgentClosed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == second.agent_id)
    })
    .await;

    let bootstrap = fixture.restart_host().await;
    let (restored_by_session, _) =
        collect_restart_replay(&mut fixture, &bootstrap, std::slice::from_ref(&session)).await;
    restored_by_session.get(&session).expect(
        "the card still open must be restored; closing its sibling must not withdraw the session",
    );
}

/// A close decides which of its sessions no other card owns, then writes that
/// decision to the store. An actor publishes its session id on its start watch
/// before it persists its restoration marker, so a resume that registers in
/// between is invisible to the decision and still leaves a marker behind for
/// the write to remove. Nothing re-marks a session during ordinary turns, so
/// the resumed card would never come back. The decision and the write are
/// therefore made under the session's resume admission claim, which is what
/// makes the resume wait rather than slip between them.
#[tokio::test]
async fn a_resume_racing_a_close_keeps_its_session_restorable() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("raced session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restore-withdraw-race".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn the card that will be closed");
    let first: NewAgentPayload = expect_next_event(&mut fixture.client, "first NewAgent")
        .await
        .parse_payload()
        .expect("parse first NewAgent");
    let start =
        expect_agent_start_on_stream(&mut fixture.client, &first.instance_stream, "first start")
            .await;
    let session = start.session_id.expect("raced session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &first.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    // The resume has to travel on its own connection. A connection routes its
    // client's frames one at a time, so a resume sent on the same one as the
    // close would not even be dispatched until the close it is racing had
    // finished. Two clients on one host is the ordinary case this race comes
    // from.
    let mut resumer = fixture.connect().await;

    // Hold the close after it has decided nobody else owns this session and
    // before it writes that decision.
    let withdraw_gate = fixture
        .host_for_test()
        .install_restore_marker_withdraw_test_gate()
        .await;
    fixture
        .client
        .close_agent(&first.instance_stream)
        .await
        .expect("close the first card");
    withdraw_gate.wait_until_entered().await;

    // Resume the same session into that window. Without the claim it registers
    // and persists its marker before the held write erases it.
    resumer
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the session the close is withdrawing");
    // The claim makes this resume wait, so its start may never arrive here. A
    // bounded wait keeps the unclaimed build deterministic without making the
    // claimed build depend on a sleep being long enough.
    let raced_agent = first.agent_id.clone();
    let raced = tokio::time::timeout(
        Duration::from_secs(3),
        fixture::next_frame_matching_on(&mut resumer, "raced NewAgent", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|payload| payload.agent_id != raced_agent)
        }),
    )
    .await
    .ok();
    if raced.is_some() {
        // Let the actor's own startup persist land before the held write runs.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // One permit for the held decision, one for the re-clear this close runs
    // after its actor stops. The second also holds the claim, so the resume
    // stays blocked until both have passed.
    withdraw_gate.release_one();
    withdraw_gate.release_one();

    let second: NewAgentPayload = match raced {
        Some(frame) => frame.parse_payload().expect("parse raced NewAgent"),
        None => fixture::next_frame_matching_on(&mut resumer, "second NewAgent", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|payload| payload.agent_id != raced_agent)
        })
        .await
        .parse_payload()
        .expect("parse second NewAgent"),
    };
    expect_agent_start_on_stream(&mut resumer, &second.instance_stream, "second start").await;

    // Closing the gate lets the shutdown inside the restart run unheld.
    drop(withdraw_gate);
    let bootstrap = fixture.restart_host().await;
    let (restored_by_session, _) =
        collect_restart_replay(&mut fixture, &bootstrap, std::slice::from_ref(&session)).await;
    restored_by_session
        .get(&session)
        .expect("a resume that raced the close must keep its session restorable across a restart");
}

/// A close of one card must not wait on another card's resume. The claim that
/// makes the ownership decision atomic is deliberately not the resume
/// admission claim: that one is held from the start of a resume through
/// backend discovery, and discovery has no deadline, so a close that waited
/// on it could be held open for as long as an unrelated provider stays
/// unresponsive. Closing a card is something a user sits in front of.
#[tokio::test]
async fn a_close_does_not_wait_on_a_held_resume_of_its_session() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("closed while resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restore-close-liveness".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn the card that will be closed");
    let card: NewAgentPayload = expect_next_event(&mut fixture.client, "card NewAgent")
        .await
        .parse_payload()
        .expect("parse card NewAgent");
    let start =
        expect_agent_start_on_stream(&mut fixture.client, &card.instance_stream, "card start")
            .await;
    let session = start.session_id.expect("card session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &card.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    // Stall a resume of the same session where a real one stalls: holding its
    // admission claim, with its backend discovery outstanding.
    let admission_gate = fixture
        .host_for_test()
        .install_resume_admission_test_gate()
        .await;
    let mut resumer = fixture.connect().await;
    resumer
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the session the close shares");
    admission_gate.wait_until_entered().await;

    fixture
        .client
        .close_agent(&card.instance_stream)
        .await
        .expect("close the card");
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture::next_frame_matching_on(&mut fixture.client, "card AgentClosed", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|payload| payload.agent_id == card.agent_id)
        }),
    )
    .await
    .expect("a close must not wait for another card's held resume to finish");

    // The connection routes one frame at a time, so a close that blocked would
    // take every later request from that client with it.
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions after the close");
    tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_session_list(&mut fixture.client, "session list after close"),
    )
    .await
    .expect("a client must still be answered while a resume is held");

    admission_gate.release_one();
}

/// A resume publishes that it owns a session before its actor exists, because
/// the actor persists that session's restoration marker before the host
/// learns the binding a close reads. A resume whose startup fails never gets
/// an actor or a binding, so it has nothing left to protect: if it kept
/// answering for the session, the close of the healthy card would find an
/// owner that will never exist, skip the marker, and the restart would reopen
/// a session the user closed everywhere.
#[tokio::test]
async fn a_failed_resume_stops_owning_the_session_it_could_not_open() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("healthy card".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restore-failed-resume".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn the healthy card");
    let healthy: NewAgentPayload = expect_next_event(&mut fixture.client, "healthy NewAgent")
        .await
        .parse_payload()
        .expect("parse healthy NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &healthy.instance_stream,
        "healthy start",
    )
    .await;
    let session = start.session_id.expect("healthy session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &healthy.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    // Hold the failing resume's spawn where one waits after its card is
    // visible and before the host authorises the session binding, which is the
    // window a failed spawn sits in while the user closes the cards.
    let publication_gate = fixture.install_spawn_operation_publication_test_gate();
    let failing_launch = fixture
        .host_for_test()
        .reserve_next_mock_spawn_failure("failed resume", "mock backend forced spawn failure")
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("failed resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the session with a backend that cannot start");
    let healthy_agent = healthy.agent_id.clone();
    let failed: NewAgentPayload =
        fixture::next_frame_matching_on(&mut fixture.client, "failed resume NewAgent", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|payload| payload.agent_id != healthy_agent)
        })
        .await
        .parse_payload()
        .expect("parse failed resume NewAgent");
    publication_gate.wait_until_entered().await;
    drop(failing_launch);
    // A held spawn keeps its card's attach pending, so the fatal startup error
    // reaches the client in the flushed bootstrap. The actor flushes it after
    // it has reported the failure, which is what makes the close below land
    // after the resume has given up rather than racing it.
    let bootstrap = expect_raw_event_on_stream(
        &mut fixture.client,
        &failed.instance_stream,
        FrameKind::AgentBootstrap,
        "failed resume AgentBootstrap",
    )
    .await;
    let bootstrap: AgentBootstrapPayload = bootstrap
        .parse_payload()
        .expect("parse failed resume AgentBootstrap");
    let failure = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("a failed resume must surface its fatal startup error");
    assert!(
        failure.fatal
            && failure
                .message
                .contains("mock backend forced spawn failure"),
        "the resume must have failed for good before the cards are closed: {failure:?}"
    );

    fixture
        .client
        .close_agent(&healthy.instance_stream)
        .await
        .expect("close the healthy card");
    fixture::next_frame_matching_on(&mut fixture.client, "healthy AgentClosed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == healthy.agent_id)
    })
    .await;
    fixture
        .client
        .close_agent(&failed.instance_stream)
        .await
        .expect("close the failed card");
    fixture::next_frame_matching_on(&mut fixture.client, "failed resume AgentClosed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == failed.agent_id)
    })
    .await;

    publication_gate.release_one();
    drop(publication_gate);

    // The replacement host reports when its restoration pass has finished, so
    // the count below is taken once everything it was going to reopen is open.
    let restoration_finished = server::new_spawn_operation_test_gate();
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        fixture.store_dir().join("sessions.json"),
        fixture.store_dir().join("projects.json"),
        fixture.store_dir().join("settings.json"),
        server::HostRuntimeConfig {
            skip_real_backend_probe: true,
            restoration_complete_test_gate: Some(restoration_finished.shared()),
            ..Default::default()
        },
    )
    .expect("spawn replacement host");
    restoration_finished.wait_until_entered().await;
    let owners = host
        .live_agent_session_ids()
        .await
        .into_iter()
        .filter(|id| id == &session)
        .count();
    assert_eq!(
        owners, 0,
        "a session whose cards were all closed must not come back because a resume failed to open it"
    );
}

/// Restoration reads the store once and then reconstructs cards one at a
/// time, so its snapshot ages while the pass runs. A card the user closes in
/// that window has already had its restoration intent withdrawn, and the
/// stale snapshot still says the session wants restoring. Rebuilding it from
/// the snapshot alone reopens an agent the user explicitly closed, so the
/// marker is re-read once the session's admission claim is held.
#[tokio::test]
async fn restoration_skips_a_session_closed_while_its_pass_was_held() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("closed mid-pass".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restore-stale-snapshot".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn the agent that will be closed mid-pass");
    let agent: NewAgentPayload = expect_next_event(&mut fixture.client, "original NewAgent")
        .await
        .parse_payload()
        .expect("parse original NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "original start",
    )
    .await;
    let session = start.session_id.expect("original session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    // The replacement host takes its restoration snapshot, which still marks
    // this session, and is then held before it reconstructs anything.
    let restoration_gate = server::new_spawn_operation_test_gate();
    let restoration_finished = server::new_spawn_operation_test_gate();
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        fixture.store_dir().join("sessions.json"),
        fixture.store_dir().join("projects.json"),
        fixture.store_dir().join("settings.json"),
        server::HostRuntimeConfig {
            skip_real_backend_probe: true,
            restoration_snapshot_test_gate: Some(restoration_gate.shared()),
            restoration_complete_test_gate: Some(restoration_finished.shared()),
            ..Default::default()
        },
    )
    .expect("spawn replacement host");
    let (mut client, _) = fixture::connect_host(host.clone()).await;
    // The snapshot is only stale if it was taken first. Waiting for the pass to
    // reach the hold is what establishes that the close below happens after it.
    restoration_gate.wait_until_entered().await;

    // Open the session by hand and close it again, entirely inside the window
    // the held pass is stalled in. The close withdraws the marker the snapshot
    // still carries.
    client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the session while restoration is held");
    let resumed: NewAgentPayload = expect_next_event(&mut client, "resumed NewAgent")
        .await
        .parse_payload()
        .expect("parse resumed NewAgent");
    expect_agent_start_on_stream(&mut client, &resumed.instance_stream, "resumed start").await;
    client
        .close_agent(&resumed.instance_stream)
        .await
        .expect("close the resumed card");
    fixture::next_frame_matching_on(&mut client, "resumed AgentClosed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == resumed.agent_id)
    })
    .await;

    restoration_gate.release_one();

    // The pass reports when it has finished, so the count below is taken after
    // it has done everything it is going to do. Without the re-read it rebuilds
    // the session from the stale snapshot and the closed card comes back.
    restoration_finished.wait_until_entered().await;
    let owners = host
        .live_agent_session_ids()
        .await
        .into_iter()
        .filter(|id| id == &session)
        .count();
    assert_eq!(
        owners, 0,
        "restoration must not reopen a session whose close completed while the pass was held"
    );
}

#[tokio::test]
async fn restoration_does_not_duplicate_a_session_a_client_is_resuming() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("contended session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restore-race".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn contended agent");
    let agent: NewAgentPayload = expect_next_event(&mut fixture.client, "contended NewAgent")
        .await
        .parse_payload()
        .expect("parse contended NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "contended start",
    )
    .await;
    let session = start.session_id.expect("contended session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "mock backend response to: hello",
    )
    .await;

    // The replacement host starts with restoration held, so the client can get
    // its resume into exactly the window the claim protects.
    let restoration_gate = server::new_spawn_operation_test_gate();
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        fixture.store_dir().join("sessions.json"),
        fixture.store_dir().join("projects.json"),
        fixture.store_dir().join("settings.json"),
        server::HostRuntimeConfig {
            skip_real_backend_probe: true,
            restoration_snapshot_test_gate: Some(restoration_gate.shared()),
            ..Default::default()
        },
    )
    .expect("spawn replacement host");

    let resume_gate = host.install_resume_admission_test_gate().await;
    let (mut client, _) = fixture::connect_host(host.clone()).await;
    client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("client resume during restoration");
    // The resume now holds the session claim and has not registered an actor.
    resume_gate.wait_until_entered().await;

    restoration_gate.release_one();
    // Let restoration reach this session and block on the claim rather than
    // racing past it. Releasing the resume first would hide the bug.
    tokio::time::sleep(Duration::from_millis(300)).await;
    resume_gate.release_one();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let owners = host
                .live_agent_session_ids()
                .await
                .into_iter()
                .filter(|id| id == &session)
                .count();
            if owners == 1 {
                // Hold it for long enough that a second owner appearing late
                // still fails the assertion below rather than slipping through.
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the resumed session should end up with exactly one owner");

    let owners = host
        .live_agent_session_ids()
        .await
        .into_iter()
        .filter(|id| id == &session)
        .count();
    assert_eq!(
        owners, 1,
        "restoration must reuse the agent a concurrent resume already claimed"
    );
}

#[tokio::test]
async fn restart_without_an_ambient_runtime_restores_working_agents() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("survives a runtimeless restart".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-no-runtime".to_owned()],
                prompt: "remember me".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent that will be restored");
    let agent: NewAgentPayload = expect_next_event(&mut fixture.client, "survivor NewAgent")
        .await
        .parse_payload()
        .expect("parse survivor NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "survivor start",
    )
    .await;
    let session = start.session_id.expect("survivor session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "mock backend response to: remember me",
    )
    .await;

    let session_path = fixture.store_dir().join("sessions.json");
    let project_path = fixture.store_dir().join("projects.json");
    let settings_path = fixture.store_dir().join("settings.json");
    let host = std::thread::spawn(move || {
        server::spawn_host_with_mock_backend_and_runtime_config(
            session_path,
            project_path,
            settings_path,
            server::HostRuntimeConfig {
                skip_real_backend_probe: true,
                ..Default::default()
            },
        )
    })
    .join()
    .expect("host construction thread")
    .expect("construct replacement host off-runtime");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if host.live_agent_session_ids().await.contains(&session) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restoration should reconstruct the open agent");

    let (mut client, bootstrap) = fixture::connect_host(host.clone()).await;
    let restored = bootstrap
        .agents
        .iter()
        .find(|restored| restored.session_id.as_ref() == Some(&session))
        .expect("restored agent in bootstrap")
        .clone();

    // The restored card is only worth anything if its actor is still alive to
    // answer. A dropped restoration runtime leaves the card and loses the
    // actor, which is exactly the failure this asserts against.
    client
        .send_message(&restored.instance_stream, "are you alive".to_owned())
        .await
        .expect("send to restored agent");
    fixture::next_frame_matching_on(&mut client, "restored agent reply", |env| {
        env.stream == restored.instance_stream
            && env.kind == FrameKind::ChatEvent
            && matches!(
                env.parse_payload::<ChatEvent>(),
                Ok(ChatEvent::StreamDelta(delta))
                    if delta.text.contains("mock backend response to: are you alive")
            )
    })
    .await;
}

#[tokio::test]
async fn restart_keeps_a_read_only_agent_read_only() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("read-only survivor".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-read-only".to_owned()],
                prompt: "stay read only".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        })
        .await
        .expect("spawn read-only agent");
    let agent: NewAgentPayload = expect_next_event(&mut fixture.client, "read-only NewAgent")
        .await
        .parse_payload()
        .expect("parse read-only NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "read-only start",
    )
    .await;
    let session = start.session_id.expect("read-only session id");
    expect_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "mock backend response to: stay read only",
    )
    .await;
    assert_eq!(
        stored_access_mode(&fixture, &session),
        BackendAccessMode::ReadOnly,
        "spawning read-only must persist the restriction"
    );

    let bootstrap = fixture.restart_host().await;
    let (restored_by_session, _) =
        collect_restart_replay(&mut fixture, &bootstrap, std::slice::from_ref(&session)).await;
    restored_by_session
        .get(&session)
        .expect("read-only agent restored after restart");

    assert_eq!(
        stored_access_mode(&fixture, &session),
        BackendAccessMode::ReadOnly,
        "restoring an open agent must not relax its read-only mode"
    );
}

#[tokio::test]
async fn restart_restores_a_backend_continued_turn_as_running() {
    for authoritative in [true, false] {
        let mut fixture = Fixture::new().await;
        let replay = server::backend::mock::MockGateHandle::new();
        let finish = server::backend::mock::MockGateHandle::new();
        let source = fixture
            .spawn_scripted(
                "continued on resume",
                MockScript::one(MockTurn::text("completed history"))
                    .with_resume_continuation(&replay, &finish),
            )
            .await;
        fixture
            .next_chat_event_matching(&source, "seed turn idle", |event| {
                matches!(event, ChatEvent::TypingStatusChanged(false))
            })
            .await;
        let session_id = fixture
            .agent_session_ids()
            .await
            .into_iter()
            .next()
            .expect("source session");
        if !authoritative {
            let journal = transcript_journal_path(&fixture, &session_id);
            eprintln!(
                "RESUME FIXTURE before persistence barrier: marker_present={} journal_present={}",
                journal.with_extension("authoritative").is_file(),
                journal.is_file(),
            );
            // The actor publishes idle before awaiting its authoritative-marker
            // write. A mailbox round trip finishes that event's persistence
            // before this fixture deliberately removes the saved transcript.
            fixture.mock(&source).await;
            eprintln!(
                "RESUME FIXTURE after persistence barrier: marker_present={} journal_present={}",
                journal.with_extension("authoritative").is_file(),
                journal.is_file(),
            );
            std::fs::remove_file(journal.with_extension("authoritative"))
                .expect("remove authoritative marker to exercise provider history");
            std::fs::remove_file(journal).expect("remove authoritative transcript");
        }

        let restarted = fixture.restart_host().await;
        replay.wait_until_entered().await;
        let restored_id = fixture
            .agent_ids()
            .await
            .into_iter()
            .next()
            .expect("restored agent registered");
        // NewAgent can truthfully advertise startup as active before the actor
        // initializes. A mailbox round trip observes initialization, not replay.
        fixture.mock_by_id(&restored_id).await;
        let (mut mobile, mobile_initial) = fixture::connect_mobile_client_with_bootstrap(
            fixture.host_for_test(),
            "resume-status-phone",
        )
        .await;
        let restored = match mobile_initial.agents.first() {
            Some(agent) => agent.clone(),
            None => fixture::next_frame_matching_on(&mut mobile, "restored NewAgent", |env| {
                env.kind == FrameKind::NewAgent
            })
            .await
            .parse_payload::<NewAgentPayload>()
            .expect("parse restored NewAgent"),
        };
        assert!(!restored.turn_active, "replay alone is not a live turn");
        replay.release_one();
        finish.wait_until_entered().await;
        let (agents, bootstraps) =
            collect_restart_replay(&mut fixture, &restarted, std::slice::from_ref(&session_id))
                .await;
        let eager = agents.get(&session_id).expect("restored eager agent");
        let bootstrap = &bootstraps[&eager.instance_stream];
        assert!(
            bootstrap.events.iter().all(|event| !matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamReasoningDelta(_))
            )),
            "post-replay reasoning must not be swallowed into history"
        );

        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            "continued turn typing before stream",
        )
        .await;
        assert!(
            matches!(event, ChatEvent::TypingStatusChanged(true)),
            "resumed live turn must publish typing(true) before its stream"
        );
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            "continued StreamStart",
        )
        .await;
        assert!(matches!(event, ChatEvent::StreamStart(_)));
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            "continued reasoning",
        )
        .await;
        assert!(matches!(event, ChatEvent::StreamReasoningDelta(_)));

        let state =
            fixture::next_frame_matching_on(&mut mobile, "continued turn host liveness", |env| {
                assert!(
                    !env.stream.0.starts_with("/agent/"),
                    "unattached mobile must not receive agent history or live output"
                );
                env.kind == FrameKind::AgentTurnStateNotify
                    && env
                        .parse_payload::<protocol::AgentTurnStateNotifyPayload>()
                        .expect("parse turn state")
                        .agent_id
                        == restored.agent_id
            })
            .await
            .parse_payload::<protocol::AgentTurnStateNotifyPayload>()
            .expect("parse running state");
        assert!(
            state.turn_active,
            "continued turn must announce running to mobile"
        );

        let (mut late_mobile, late_host) = fixture::connect_mobile_client_with_bootstrap(
            fixture.host_for_test(),
            "late-resume-status-phone",
        )
        .await;
        let late = late_host
            .agents
            .iter()
            .find(|agent| agent.agent_id == restored.agent_id)
            .expect("late mobile descriptor");
        assert!(
            late.turn_active,
            "HostBootstrap/NewAgent descriptor must report running"
        );
        fixture::send_load_agent_on(&mut late_mobile, &late.instance_stream).await;
        let late_bootstrap = fixture::next_frame_matching_on(
            &mut late_mobile,
            "continued turn lazy bootstrap",
            |env| env.kind == FrameKind::AgentBootstrap && env.stream == late.instance_stream,
        )
        .await
        .parse_payload::<AgentBootstrapPayload>()
        .expect("parse lazy bootstrap");
        assert!(
            late_bootstrap.turn_active,
            "AgentBootstrap must preserve the active turn"
        );
        assert!(
            late_bootstrap.events.iter().any(|event| matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamReasoningDelta(_))
            )),
            "late attach must retain the active reasoning stream"
        );
        let control = fixture.connect_agent_control().await;
        let awaiting = control.await_agents(Some(vec![restored.agent_id.clone()]));
        tokio::pin!(awaiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut awaiting)
                .await
                .is_err(),
            "agent-control await must stay pending while the resumed turn is thinking"
        );
        expect_no_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            Duration::from_millis(100),
            "continued turn must not publish idle or replay history before release",
        )
        .await;

        finish.release_one();
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            "continued StreamEnd",
        )
        .await;
        assert!(matches!(event, ChatEvent::StreamEnd(_)));
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &eager.instance_stream,
            "continued idle after end",
        )
        .await;
        assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
        let state =
            fixture::next_frame_matching_on(&mut mobile, "continued turn finished", |env| {
                env.kind == FrameKind::AgentTurnStateNotify
                    && env
                        .parse_payload::<protocol::AgentTurnStateNotifyPayload>()
                        .expect("parse turn state")
                        .agent_id
                        == restored.agent_id
            })
            .await
            .parse_payload::<protocol::AgentTurnStateNotifyPayload>()
            .expect("parse idle state");
        assert!(!state.turn_active);
        let settled = tokio::time::timeout(Duration::from_secs(5), awaiting)
            .await
            .expect("agent-control await completes after the turn")
            .expect("agent-control await succeeds");
        assert_eq!(settled.ready.len(), 1);
        assert_eq!(settled.ready[0].status, protocol::AgentControlStatus::Idle);
        assert!(settled.still_thinking.is_empty());
    }
}

#[tokio::test]
async fn restart_restores_open_agents_and_preserves_settings() {
    let mut fixture = Fixture::new().await;
    // The failing child replay contained StreamStart/Delta/End at bootstrap
    // seq 0, already idle, so no later TypingStatusChanged(false) was due.
    // Hold each turn until subscription to preserve every live-turn assertion.
    let parent_gate = server::backend::mock::MockGateHandle::new();
    let child_gate = server::backend::mock::MockGateHandle::new();
    let closed_gate = server::backend::mock::MockGateHandle::new();
    let parent_reservation = fixture
        .reserve_next_mock_launch(
            "survives restart",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: remember this turn",
                &parent_gate,
            )),
        )
        .await;
    let mut settings = SessionSettingsValues::default();
    settings.0.insert(
        "effort".to_owned(),
        SessionSettingValue::String("high".to_owned()),
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("survives restart".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-survivor".to_owned()],
                prompt: "remember this turn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(settings.clone()),
            },
        })
        .await
        .expect("spawn restart survivor");
    let survivor: NewAgentPayload = expect_next_event(&mut fixture.client, "survivor NewAgent")
        .await
        .parse_payload()
        .expect("parse survivor NewAgent");
    let survivor_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &survivor.instance_stream,
        "survivor start",
    )
    .await;
    let survivor_session = survivor_start.session_id.expect("survivor session id");
    parent_gate.release_one();
    drop(parent_reservation);
    expect_turn_on_stream(
        &mut fixture.client,
        &survivor.instance_stream,
        "mock backend response to: remember this turn",
    )
    .await;

    let child_reservation = fixture
        .reserve_next_mock_launch(
            "child survives restart",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: child turn",
                &child_gate,
            )),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("child survives restart".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(survivor.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-survivor-child".to_owned()],
                prompt: "child turn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn restart survivor child");
    let survivor_child: NewAgentPayload =
        expect_next_event(&mut fixture.client, "survivor child NewAgent")
            .await
            .parse_payload()
            .expect("parse survivor child NewAgent");
    let survivor_child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &survivor_child.instance_stream,
        "survivor child start",
    )
    .await;
    let survivor_child_session = survivor_child_start
        .session_id
        .expect("survivor child session id");
    child_gate.release_one();
    drop(child_reservation);
    expect_turn_on_stream(
        &mut fixture.client,
        &survivor_child.instance_stream,
        "mock backend response to: child turn",
    )
    .await;

    let closed_reservation = fixture
        .reserve_next_mock_launch(
            "closed before restart",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: do not restore me",
                &closed_gate,
            )),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("closed before restart".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-closed".to_owned()],
                prompt: "do not restore me".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent that will be closed");
    let closed: NewAgentPayload = expect_next_event(&mut fixture.client, "closed NewAgent")
        .await
        .parse_payload()
        .expect("parse closed NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &closed.instance_stream,
        "closed agent start",
    )
    .await;
    closed_gate.release_one();
    drop(closed_reservation);
    expect_turn_on_stream(
        &mut fixture.client,
        &closed.instance_stream,
        "mock backend response to: do not restore me",
    )
    .await;
    fixture
        .client
        .close_agent(&closed.instance_stream)
        .await
        .expect("close second agent");
    fixture::next_frame_matching_on(&mut fixture.client, "closed AgentClosed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == closed.agent_id)
    })
    .await;

    let bootstrap = fixture.restart_host().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.agent_ids().await.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both open agents should be reconstructed after restart");

    let (mut restored_by_session, restored_bootstraps) = collect_restart_replay(
        &mut fixture,
        &bootstrap,
        &[survivor_session.clone(), survivor_child_session.clone()],
    )
    .await;
    let restored = restored_by_session
        .remove(&survivor_session)
        .expect("restored parent");
    let restored_child = restored_by_session
        .remove(&survivor_child_session)
        .expect("restored child");
    assert_ne!(restored.agent_id, survivor.agent_id);
    assert_ne!(restored_child.agent_id, survivor_child.agent_id);
    assert_eq!(restored_child.name, "child survives restart");
    // The durable parent session id is the only lineage that survives a
    // restart; the restored child must hang off the parent's *new* agent id.
    assert_eq!(
        restored_child.parent_agent_id.as_ref(),
        Some(&restored.agent_id),
        "restored child must be re-parented onto the restored parent agent",
    );
    assert_eq!(restored.name, "survives restart");
    assert_eq!(restored.workspace_roots, vec!["/tmp/restart-survivor"]);
    assert_eq!(restored.session_id.as_ref(), Some(&survivor_session));

    let restored_bootstrap = restored_bootstraps
        .get(&restored.instance_stream)
        .expect("restored parent AgentBootstrap");
    assert_bootstrap_tail_messages(restored_bootstrap, &["remember this turn"]);
    let restored_child_bootstrap = restored_bootstraps
        .get(&restored_child.instance_stream)
        .expect("restored child AgentBootstrap");
    assert_bootstrap_tail_messages(restored_child_bootstrap, &["child turn"]);
    let restored_settings = restored_bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::SessionSettings(payload) => Some(&payload.values),
            _ => None,
        })
        .expect("restored bootstrap includes session settings");
    assert_eq!(restored_settings, &settings);
    let mut host = fixture.host_for_test();
    for enabled in [false, true] {
        let write_id = protocol::SettingsWriteId(format!("resume-previous-{enabled}"));
        fixture
            .client
            .settings_write(protocol::SettingsWritePayload {
                write_id: write_id.clone(),
                ops: vec![protocol::SettingOp::Replace {
                    path: "/resume_previous_agents".to_owned(),
                    value: serde_json::json!(enabled),
                    expected: protocol::SettingExpectation::Value {
                        value: serde_json::json!(!enabled),
                    },
                }],
            })
            .await
            .expect("change automatic restoration preference");
        let result = fixture::next_frame_matching_on(
            &mut fixture.client,
            "restoration preference saved",
            |env| {
                env.kind == FrameKind::SettingsWriteResult
                    && env
                        .parse_payload::<protocol::SettingsWriteResultPayload>()
                        .is_ok_and(|result| result.write_id == write_id)
            },
        )
        .await
        .parse_payload::<protocol::SettingsWriteResultPayload>()
        .expect("parse setting result");
        assert!(
            result.applied && result.field_errors.is_empty(),
            "restoration preference write must succeed"
        );

        host.shutdown_agents_for_conformance().await;
        let finished = server::new_spawn_operation_test_gate();
        host = server::spawn_host_with_mock_backend_and_runtime_config(
            fixture.store_dir().join("sessions.json"),
            fixture.store_dir().join("projects.json"),
            fixture.store_dir().join("settings.json"),
            server::HostRuntimeConfig {
                restoration_complete_test_gate: Some(finished.shared()),
                skip_real_backend_probe: true,
                ..Default::default()
            },
        )
        .expect("restart with persisted restoration preference");
        finished.wait_until_entered().await;
        let (client, bootstrap) = fixture::connect_host(host.clone()).await;
        finished.release_one();
        fixture.client = client;
        assert_eq!(bootstrap.settings.resume_previous_agents, enabled);
        assert_eq!(
            bootstrap.agents.len(),
            if enabled { 2 } else { 0 },
            "only enabled startup may restore the open hierarchy"
        );
        if enabled {
            let parent = bootstrap
                .agents
                .iter()
                .find(|agent| agent.session_id.as_ref() == Some(&survivor_session))
                .expect("restored parent after re-enabling");
            let child = bootstrap
                .agents
                .iter()
                .find(|agent| agent.session_id.as_ref() == Some(&survivor_child_session))
                .expect("restored child after re-enabling");
            assert!(
                child.parent_agent_id.as_ref() == Some(&parent.agent_id),
                "re-enabled restoration preserves ownership"
            );
            continue;
        }
        fixture
            .client
            .list_sessions(ListSessionsPayload {
                scope: None,
                cursor: None,
                limit: None,
            })
            .await
            .expect("list saved sessions while automatic resume is off");
        let sessions = fixture::next_frame_matching_on(
            &mut fixture.client,
            "saved history with restoration disabled",
            |env| env.kind == FrameKind::SessionList,
        )
        .await
        .parse_payload::<SessionListPayload>()
        .expect("parse saved sessions");
        assert!(
            sessions
                .sessions
                .iter()
                .any(|session| session.id == survivor_session),
            "disabled restoration retains parent history"
        );
        assert!(
            sessions
                .sessions
                .iter()
                .any(|session| session.id == survivor_child_session),
            "disabled restoration retains child history"
        );
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                name: None,
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id: survivor_child_session.clone(),
                    prompt: None,
                },
            })
            .await
            .expect("manually resume a child while automatic restoration is disabled");
        let child =
            fixture::next_frame_matching_on(&mut fixture.client, "manual child resume", |env| {
                env.kind == FrameKind::NewAgent
                    && env.parse_payload::<NewAgentPayload>().is_ok_and(|agent| {
                        agent.session_id.as_ref() == Some(&survivor_child_session)
                    })
            })
            .await
            .parse_payload::<NewAgentPayload>()
            .expect("parse manual child");
        expect_agent_start_on_stream(
            &mut fixture.client,
            &child.instance_stream,
            "manual child start",
        )
        .await;
        let (_, manual) = fixture::connect_host(host.clone()).await;
        assert_eq!(
            manual.agents.len(),
            2,
            "manual child resume restores its owning parent only"
        );
        let parent = manual
            .agents
            .iter()
            .find(|agent| agent.session_id.as_ref() == Some(&survivor_session))
            .expect("manually restored owner");
        assert!(
            child.parent_agent_id.as_ref() == Some(&parent.agent_id),
            "manual resume retains ownership with automatic restoration off"
        );
    }
}

#[tokio::test]
async fn opening_agent_bootstrap_loads_tail_and_gates_older_history() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("history-on-demand".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/history-on-demand".to_owned()],
                prompt: "history 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn history agent failed");

    let env = expect_next_event(&mut fixture.client, "history NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse history NewAgent");
    let _ = expect_next_event(&mut fixture.client, "history AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: history 0").await;

    for index in 1..55 {
        let prompt = format!("history {index}");
        fixture
            .client
            .send_message(&new_agent.instance_stream, prompt.clone())
            .await
            .expect("send history follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_agent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running history agent");

    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_agent_stream,
        FrameKind::AgentBootstrap,
        "history AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let gate_before_seq = assert_bootstrap_prior_history_indicator(&payload, 40);
    let expected_tail_strings = (40..55)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    let first_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(gate_before_seq),
        2,
    )
    .await;
    assert_history_page(&first_page, &["history 38", "history 39"], true);
    let first_cursor = first_page
        .oldest_seq
        .expect("first history page should include an oldest_seq cursor");

    let second_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(first_cursor),
        2,
    )
    .await;
    assert_history_page(&second_page, &["history 36", "history 37"], true);
    let second_cursor = second_page
        .oldest_seq
        .expect("second history page should include an oldest_seq cursor");

    let third_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(second_cursor),
        50,
    )
    .await;
    let expected_final_strings = (0..=35)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_final = expected_final_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_history_page(&third_page, &expected_final, false);
}

#[tokio::test]
async fn first_history_fetch_uses_bootstrap_gate_cursor_without_live_dupes() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("history-no-dupe".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/history-no-dupe".to_owned()],
                prompt: "prior 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn history no-dupe agent failed");

    let env = expect_next_event(&mut fixture.client, "no-dupe NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse no-dupe NewAgent");
    let _ = expect_next_event(&mut fixture.client, "no-dupe AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: prior 0").await;
    for index in 1..51 {
        let prompt = format!("prior {index}");
        fixture
            .client
            .send_message(&new_agent.instance_stream, prompt.clone())
            .await
            .expect("send history no-dupe follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_agent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running history agent");

    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_agent_stream,
        FrameKind::AgentBootstrap,
        "no-dupe AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let gate_before_seq = assert_bootstrap_prior_history_indicator(&payload, 36);
    let expected_tail_strings = (36..51)
        .map(|index| format!("prior {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    second_client
        .send_message(&second_agent_stream, "new visible message".to_owned())
        .await
        .expect("send visible follow-up failed");
    expect_turn_on_stream(
        &mut second_client,
        &second_agent_stream,
        "mock backend response to: new visible message",
    )
    .await;

    let first_page = fetch_history_page(
        &mut second_client,
        &second_agent_stream,
        new_agent.agent_id.clone(),
        Some(gate_before_seq),
        10,
    )
    .await;
    assert_history_page(
        &first_page,
        &[
            "prior 26", "prior 27", "prior 28", "prior 29", "prior 30", "prior 31", "prior 32",
            "prior 33", "prior 34", "prior 35",
        ],
        true,
    );
    assert!(
        first_page.events.iter().all(|event| match event {
            ChatEvent::MessageAdded(message) => !message.content.contains("new visible message"),
            ChatEvent::StreamDelta(delta) => !delta.text.contains("new visible message"),
            ChatEvent::StreamEnd(end) => !end.message.content.contains("new visible message"),
            _ => true,
        }),
        "first history fetch must not duplicate live rows: {:?}",
        first_page.events
    );
}

#[tokio::test]
async fn resume_long_replay_history_stays_capped_without_live_broadcast() {
    // Regression: restoring a long stored session must bootstrap the resuming
    // client with only the 15-message tail (plus the prior-history indicator),
    // never live-broadcast the entire replayed transcript. The single-message
    // sibling test below drains one event before the resume-replay barrier is
    // selected, so it cannot catch the race where the barrier closes the gate
    // while many replay events are still buffered on the backend stream.
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-long-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-long".to_owned()],
                prompt: "history 0".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume long source failed");

    let env = expect_next_event(&mut fixture.client, "resume long source NewAgent").await;
    let source: NewAgentPayload = env
        .parse_payload()
        .expect("parse resume long source NewAgent");
    let _ = expect_next_event(&mut fixture.client, "resume long source AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: history 0").await;

    // Build a 30-message transcript so many replay events are buffered at once.
    for index in 1..30 {
        let prompt = format!("history {index}");
        fixture
            .client
            .send_message(&source.instance_stream, prompt.clone())
            .await
            .expect("send resume long follow-up failed");
        expect_turn(
            &mut fixture.client,
            &format!("mock backend response to: {prompt}"),
        )
        .await;
    }

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before long resume failed");
    let list = wait_for_session_list(&mut fixture.client, "SessionList before long resume").await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-long".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume long agent failed");

    let env = expect_next_event(&mut fixture.client, "resume long NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resume long NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resume long AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");

    // 30 replayed messages -> 15 gated behind the prior-history indicator, 15 in the tail.
    assert_bootstrap_prior_history_indicator(&payload, 15);
    let expected_tail_strings = (15..30)
        .map(|index| format!("history {index}"))
        .collect::<Vec<_>>();
    let expected_tail = expected_tail_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_bootstrap_tail_messages(&payload, &expected_tail);

    // The gated (older) portion of the transcript must not leak as live events.
    expect_no_chat_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        Duration::from_millis(300),
        "replayed history must not live-broadcast",
    )
    .await;
}

#[tokio::test]
async fn async_resume_replay_history_is_ingested_without_live_broadcast() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-no-leak-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-no-leak".to_owned()],
                prompt: "original history".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume no-leak source failed");

    let _ = expect_next_event(&mut fixture.client, "resume source NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "resume source AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: original history",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before resume failed");
    let list =
        wait_for_session_list(&mut fixture.client, "SessionList before no-leak resume").await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-no-leak".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume no-leak agent failed");

    let env = expect_next_event(&mut fixture.client, "resume no-leak NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resume no-leak NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resume no-leak AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &["original history"]);

    expect_no_chat_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        Duration::from_millis(150),
        "new turn after quiet resume",
    )
    .await;

    fixture
        .client
        .send_message(&resumed.instance_stream, "new turn after resume".to_owned())
        .await
        .expect("send after no-leak resume failed");
    expect_turn_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        "mock backend response to: new turn after resume",
    )
    .await;
}

#[tokio::test]
async fn resume_replay_deadline_rejects_ready_history_and_late_boundary() {
    for late_boundary in [true, false] {
        let mut fixture = Fixture::new().await;
        let replay = server::backend::mock::MockResumeReplay::default();
        let source = fixture
            .spawn_scripted(
                "replay deadline",
                MockScript::one(MockTurn::text("completed history"))
                    .with_controlled_resume_replay(&replay),
            )
            .await;
        fixture.finish_turn(&source).await;
        let restarted = fixture.restart_host().await;
        let restored_id = if let Some(agent) = restarted.agents.first() {
            agent.agent_id.clone()
        } else {
            fixture::next_frame_matching_on(&mut fixture.client, "restored NewAgent", |env| {
                env.kind == FrameKind::NewAgent
            })
            .await
            .parse_payload::<NewAgentPayload>()
            .expect("parse restored NewAgent")
            .agent_id
        };
        replay.wait_until_started().await;
        fixture.mock_by_id(&restored_id).await;

        tokio::time::pause();

        // Each burst stays ready across the clock advance. A late marker behind
        // the final burst must not turn an expired replay into a successful attach.
        for _ in 0..2 {
            replay.history_batch(8);
            tokio::time::advance(Duration::from_secs(10)).await;
            fixture.mock_by_id(&restored_id).await;
        }
        replay.history_batch(8);
        if late_boundary {
            replay.complete();
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        let env = fixture::next_frame_matching_on(
            &mut fixture.client,
            "expired replay eager bootstrap",
            |env| env.kind == FrameKind::AgentBootstrap,
        )
        .await;
        let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
        let error = bootstrap.events.iter().find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        });
        assert!(
            error.is_some(),
            "ready replay history and a late boundary must not bypass the 30s deadline"
        );
        let error = error.expect("fatal replay timeout");
        assert!(error.fatal);
        assert_eq!(error.code, protocol::AgentErrorCode::BackendFailed);
        assert_eq!(
            error.message,
            "failed to resume agent history before live replay boundary: timed out after 30s waiting for resume replay to complete"
        );
        assert_bootstrap_has_no_prior_history_indicator(&bootstrap);
        // The fatal bootstrap retains the saved authoritative StreamEnd.
        // Require that completed turn, not the unfinished provider replay.
        let messages = bootstrap_message_contents(&bootstrap);
        assert_eq!(messages.len(), 1, "only the saved turn belongs in history");
        assert_eq!(
            messages[0], "completed history",
            "saved history must survive timeout"
        );
        assert!(
            !bootstrap.turn_active,
            "timed-out replay must not be running"
        );
        tokio::time::resume();
    }
}

#[tokio::test]
async fn resume_backend_close_before_barrier_flushes_eager_attach_with_fatal_error() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-close-before-barrier-source".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-close-before-barrier".to_owned()],
                prompt: "close before barrier seed".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn resume close-before-barrier source failed");

    let _ = expect_next_event(&mut fixture.client, "close-before-barrier source NewAgent").await;
    let _ = expect_next_event(
        &mut fixture.client,
        "close-before-barrier source AgentStart",
    )
    .await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: close before barrier seed",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions before close-before-barrier resume failed");
    let list = wait_for_session_list(
        &mut fixture.client,
        "SessionList before close-before-barrier resume",
    )
    .await;
    let session_id = list
        .sessions
        .first()
        .expect("expected source session")
        .id
        .clone();

    let close_reservation = fixture
        .reserve_next_mock_resume_closing_before_barrier("resume-close-before-barrier")
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-close-before-barrier".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume close-before-barrier agent failed");

    let env = expect_next_event(&mut fixture.client, "close-before-barrier resumed NewAgent").await;
    drop(close_reservation);
    let resumed: NewAgentPayload = env
        .parse_payload()
        .expect("parse close-before-barrier resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "close-before-barrier AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let error = payload
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("AgentBootstrap should surface fatal resume barrier error");
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("agent backend closed before resume replay completed"),
        "unexpected fatal error: {}",
        error.message
    );
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &[]);
}

#[tokio::test]
async fn agent_bootstrap_keeps_active_stream_while_recent_history_loads() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("active-history-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/active-history-parent".to_owned()],
                prompt: "parent ready".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "active parent NewAgent").await;
    let parent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent.instance_stream,
        "active parent AgentStart",
    )
    .await;
    expect_turn_on_stream(
        &mut fixture.client,
        &parent.instance_stream,
        "mock backend response to: parent ready",
    )
    .await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "active-history-child",
            MockScript::one(MockTurn::held_text("child active held open")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("active-history-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/active-history-child".to_owned()],
                prompt: "child active".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let env = expect_next_event(&mut fixture.client, "active child NewAgent").await;
    let child: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "active child AgentStart",
    )
    .await;
    loop {
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &child.instance_stream,
            "held child StreamEnd",
        )
        .await;
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }
    drop(reservation);

    fixture
        .mock_by_id(&parent.agent_id)
        .await
        .enqueue(MockTurn::agent_control_await(vec![child.agent_id.clone()]))
        .await;
    fixture
        .client
        .send_message(
            &parent.instance_stream,
            format!("await child {}", child.agent_id.0),
        )
        .await
        .expect("send parent await prompt failed");

    loop {
        let event = expect_chat_event_on_stream(
            &mut fixture.client,
            &parent.instance_stream,
            "parent active ToolRequest",
        )
        .await;
        if matches!(event, ChatEvent::ToolRequest(_)) {
            break;
        }
    }

    let (mut second_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_parent_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == parent.agent_id)
        .map(|agent| agent.instance_stream.clone())
        .expect("host bootstrap must advertise the running parent agent");
    let env = expect_raw_event_on_stream(
        &mut second_client,
        &second_parent_stream,
        FrameKind::AgentBootstrap,
        "active parent AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    assert_bootstrap_has_no_prior_history_indicator(&payload);
    assert_bootstrap_tail_messages(&payload, &["parent ready"]);
    let active_start_index = payload
        .events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(_))
            )
        })
        .expect("active StreamStart should be replayed in AgentBootstrap");
    let active_events = &payload.events[active_start_index + 1..];
    assert!(
        active_events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(request))
                if fixture::tool_request_name(request) == "tyde_await_agents"
        )),
        "active tool request should be replayed in AgentBootstrap: {:?}",
        payload.events
    );
    assert!(
        active_events.iter().all(|event| !matches!(
            event,
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(_))
        )),
        "active response should remain open after its final StreamStart: {:?}",
        payload.events
    );

    fixture
        .client
        .interrupt(&child.instance_stream)
        .await
        .expect("interrupt held child");
}

#[tokio::test]
async fn session_listing_covers_empty_parent_child_and_resume_without_prompt() {
    let mut fixture = Fixture::new().await;
    // A completed bootstrap reports turn_active=false instead of replaying a
    // live idle event. Hold these turns until subscription so expect_turn
    // always checks live streaming, even when the mock finishes immediately.
    let parent_gate = server::backend::mock::MockGateHandle::new();
    let child_gate = server::backend::mock::MockGateHandle::new();
    let parent_reservation = fixture
        .reserve_next_mock_launch(
            "parent",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: parent hello",
                &parent_gate,
            )),
        )
        .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("initial list_sessions failed");

    let list = wait_for_session_list(&mut fixture.client, "initial empty SessionList").await;
    assert!(
        list.sessions.is_empty(),
        "expected no sessions before any spawn"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/parent".to_owned()],
                prompt: "parent hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    let parent_new_agent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let _ = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    parent_gate.release_one();
    drop(parent_reservation);
    expect_turn(
        &mut fixture.client,
        "mock backend response to: parent hello",
    )
    .await;

    let child_reservation = fixture
        .reserve_next_mock_launch(
            "child",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: child hello",
                &child_gate,
            )),
        )
        .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_new_agent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/child".to_owned()],
                prompt: "child hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let _ = expect_next_event(&mut fixture.client, "child NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "child AgentStart").await;
    child_gate.release_one();
    drop(child_reservation);
    expect_turn(&mut fixture.client, "mock backend response to: child hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions with parent/child failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList with parent/child").await;
    assert_eq!(
        list.sessions.len(),
        2,
        "expected two sessions in a single SessionList event"
    );

    let parent = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("parent"))
        .expect("missing parent session in SessionList");
    let child = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("child"))
        .expect("missing child session in SessionList");
    assert_eq!(
        child.parent_id.as_ref(),
        Some(&parent.id),
        "child session should point to parent session id",
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: parent.id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume without prompt failed");

    let env = expect_next_event(&mut fixture.client, "resumed parent NewAgent").await;
    let resumed_parent: NewAgentPayload =
        env.parse_payload().expect("parse resumed parent NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_parent.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed parent AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resumed parent AgentBootstrap");
    let start = bootstrap_agent_start(&payload);
    assert_eq!(start.agent_id, resumed_parent.agent_id);
    assert_bootstrap_tail_messages(&payload, &["parent hello"]);

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "resume without prompt should not start a turn",
    )
    .await;

    fixture
        .client
        .send_message(
            &resumed_parent.instance_stream,
            "after quiet resume".to_owned(),
        )
        .await
        .expect("send_message after quiet resume failed");

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after quiet resume",
    )
    .await;
}

#[tokio::test]
async fn session_project_id_persists_and_resume_can_override_it() {
    let mut fixture = Fixture::new().await;

    let project_a = create_project(
        &mut fixture.client,
        "Project A",
        vec!["/tmp/project-a".to_owned()],
    )
    .await;
    let project_b = create_project(
        &mut fixture.client,
        "Project B",
        vec!["/tmp/project-b".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("project-session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project_a.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&project_a),
                prompt: "session project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn project session failed");

    let env = expect_next_event(&mut fixture.client, "project session NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse project session NewAgent");
    assert_eq!(new_agent.project_id.as_ref(), Some(&project_a.id));

    let env = expect_next_event(&mut fixture.client, "project session AgentStart").await;
    let start: AgentStartPayload = env
        .parse_payload()
        .expect("parse project session AgentStart");
    assert_eq!(start.project_id.as_ref(), Some(&project_a.id));

    expect_turn(
        &mut fixture.client,
        "mock backend response to: session project",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after project spawn failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList after project spawn").await;
    assert_eq!(list.sessions.len(), 1);
    let session = &list.sessions[0];
    assert_eq!(session.project_id.as_ref(), Some(&project_a.id));

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-same-project".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("resume same".to_owned()),
            },
        })
        .await
        .expect("resume with stored project failed");

    let env = expect_next_event(&mut fixture.client, "resume same project NewAgent").await;
    let resumed_same: NewAgentPayload = env.parse_payload().expect("parse resumed same NewAgent");
    assert_eq!(resumed_same.project_id.as_ref(), Some(&project_a.id));
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_same.instance_stream,
        FrameKind::AgentBootstrap,
        "resume same project AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resume same AgentBootstrap");
    let resumed_same_start = bootstrap_agent_start(&payload);
    assert_eq!(resumed_same_start.project_id.as_ref(), Some(&project_a.id));
    assert_bootstrap_tail_messages(&payload, &["session project"]);
    expect_turn(&mut fixture.client, "mock backend response to: resume same").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-other-project".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project_b.id.clone()),
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("resume other".to_owned()),
            },
        })
        .await
        .expect("resume with overridden project failed");

    let env = expect_next_event(&mut fixture.client, "resume other project NewAgent").await;
    let resumed_other: NewAgentPayload = env.parse_payload().expect("parse resumed other NewAgent");
    assert_eq!(resumed_other.project_id.as_ref(), Some(&project_b.id));
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed_other.instance_stream,
        FrameKind::AgentBootstrap,
        "resume other project AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse resume other AgentBootstrap");
    let resumed_other_start = bootstrap_agent_start(&payload);
    assert_eq!(resumed_other_start.project_id.as_ref(), Some(&project_b.id));
    assert_bootstrap_tail_messages(&payload, &["session project", "resume same"]);
    expect_turn(
        &mut fixture.client,
        "mock backend response to: resume other",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after override failed");

    let list = wait_for_session_list(&mut fixture.client, "SessionList after override").await;
    assert_eq!(
        list.sessions.len(),
        1,
        "resume should still reuse one session"
    );
    assert_eq!(list.sessions[0].id, session.id);
    assert_eq!(list.sessions[0].project_id.as_ref(), Some(&project_b.id));
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: roots.into_iter().map(ProjectRootPath).collect(),
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create helper").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}

// Bug 6: Delete Session

#[tokio::test]
async fn delete_session_removes_it_from_list() {
    let mut fixture = Fixture::new().await;

    // Spawn an agent so a session gets recorded.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("to-delete".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/delete-session".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    // Confirm the session is present.
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, "initial SessionList").await;
    assert_eq!(list.sessions.len(), 1, "expected one session before delete");
    let session_id = list.sessions[0].id.clone();

    // Delete the session — server will fan-out an updated SessionList automatically.
    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: session_id.clone(),
        })
        .await
        .expect("delete_session failed");

    for attempt in 0..3 {
        let list = wait_for_session_list(&mut fixture.client, "SessionList after delete").await;
        if list.sessions.is_empty() {
            return;
        }
        if attempt == 2 {
            panic!(
                "session list must be empty after delete, got {:?}",
                list.sessions
                    .iter()
                    .map(|s| s.id.0.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn delete_nonexistent_session_is_graceful() {
    let mut fixture = Fixture::new().await;

    // Delete a session that was never created — server must not crash and must
    // emit an updated (empty) SessionList.
    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: SessionId("nonexistent-session-id".to_owned()),
        })
        .await
        .expect("delete_session write failed");

    let list = wait_for_session_list(
        &mut fixture.client,
        "SessionList after deleting nonexistent session",
    )
    .await;
    assert!(
        list.sessions.is_empty(),
        "session list should be empty; deleting a nonexistent session must be a no-op"
    );
}

#[tokio::test]
async fn requested_compactions_keep_one_row_when_observed_after_completion() {
    use protocol::{
        CompactionTrigger, ContextCompactionNotifyPayload, ContextCompactionStatus,
        ContextCompactionTimelineStatus,
    };
    use server::backend::mock::MockGateHandle;

    let mut fixture = Fixture::new().await;
    let observation_release = MockGateHandle::new();
    let observation_sent = MockGateHandle::new();
    let busy_turn = MockGateHandle::new();
    let script = MockScript::one(MockTurn::text("ready"))
        .then(MockTurn::text("after idle compaction"))
        .then(MockTurn::gated_text("busy turn finished", &busy_turn))
        .then(MockTurn::text("after busy compaction"))
        .with_late_compaction_observation(&observation_release, &observation_sent);
    let agent = fixture
        .spawn_scripted("compaction correlation", script)
        .await;
    fixture.finish_turn(&agent).await;
    let mut operations = Vec::new();

    for busy in [false, true] {
        if busy {
            fixture
                .client
                .send_message(&agent.stream, "work before compacting".to_owned())
                .await
                .expect("start busy turn");
            busy_turn.wait_until_entered().await;
        }
        fixture
            .client
            .compact_agent(&agent.stream, protocol::AgentCompactPayload::default())
            .await
            .expect("request compaction");
        let mut saw_deferred = false;
        let terminal = loop {
            let frame = fixture::next_frame_matching_on(
                &mut fixture.client,
                "requested compaction status",
                |frame| {
                    frame.stream == agent.stream && frame.kind == FrameKind::ContextCompactionNotify
                },
            )
            .await;
            let notification: ContextCompactionNotifyPayload =
                frame.parse_payload().expect("compaction notification");
            match notification.status {
                ContextCompactionStatus::Deferred { stage } => {
                    assert_eq!(stage, protocol::CompactionStage::WaitingForIdle);
                    if busy && !saw_deferred {
                        busy_turn.release_one();
                    }
                    saw_deferred = true;
                }
                ContextCompactionStatus::Completed => break notification,
                ContextCompactionStatus::Failed { .. } => {
                    panic!("compaction failed: {notification:?}")
                }
                _ => {}
            }
        };
        // Admission publishes WaitingForIdle for every request before dispatch,
        // including requests received while idle.
        assert!(saw_deferred);
        operations.push(terminal.operation_id);
        observation_release.wait_until_entered().await;
        observation_release.release_one();
        observation_sent.wait_until_entered().await;
        observation_sent.release_one();
        fixture
            .client
            .send_message(&agent.stream, "continue after compaction".to_owned())
            .await
            .expect("send follow-up");
        fixture.finish_turn(&agent).await;

        let history = fetch_history_page(
            &mut fixture.client,
            &agent.stream,
            agent.new_agent.agent_id.clone(),
            None,
            100,
        )
        .await;
        let markers: Vec<_> = history
            .events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::ContextCompaction(marker) => Some(marker),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers.len(),
            operations.len(),
            "one persisted row per requested compaction: {markers:?}"
        );
        for operation in &operations {
            let matching: Vec<_> = markers
                .iter()
                .filter(|marker| marker.operation_id.as_ref() == Some(operation))
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].trigger, CompactionTrigger::UserRequested);
            assert_eq!(
                matching[0].status,
                ContextCompactionTimelineStatus::Completed
            );
        }
    }

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list compacted session");
    let sessions = wait_for_session_list(&mut fixture.client, "compacted session").await;
    assert_eq!(sessions.sessions.len(), 1);
    let session = &sessions.sessions[0];
    let (resumed, _) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some("resumed compacted session".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: None,
            },
        })
        .await;
    let history = fetch_history_page(
        &mut fixture.client,
        &resumed.stream,
        resumed.new_agent.agent_id.clone(),
        None,
        100,
    )
    .await;
    let markers: Vec<_> = history
        .events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::ContextCompaction(marker) => Some(marker),
            _ => None,
        })
        .collect();
    assert_eq!(
        markers.len(),
        operations.len(),
        "resume must preserve each compaction exactly once"
    );
    for operation in &operations {
        assert_eq!(
            markers
                .iter()
                .filter(|marker| marker.operation_id.as_ref() == Some(operation))
                .count(),
            1
        );
    }
}

/// Where the durable transcript journal for a session lives under the fixture.
fn transcript_journal_path(fixture: &Fixture, session_id: &SessionId) -> std::path::PathBuf {
    let safe = session_id
        .0
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    fixture
        .store_dir()
        .join("transcripts")
        .join(format!("{safe}.jsonl"))
}

fn journal_lines(path: &Path) -> Vec<String> {
    let contents = std::fs::read_to_string(path).expect("read transcript journal");
    contents
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

async fn wait_for_session_id(fixture: &mut Fixture, context: &str) -> SessionId {
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let list = wait_for_session_list(&mut fixture.client, context).await;
    assert_eq!(
        list.sessions.len(),
        1,
        "expected exactly one stored session"
    );
    list.sessions[0].id.clone()
}

/// Drive a turn to its end and count the agent errors seen along the way.
///
/// A latch that re-arms itself looks exactly like a working one until you count
/// the reports across a whole turn.
async fn expect_turn_counting_agent_errors(
    client: &mut client::Connection,
    expected_text: &str,
) -> usize {
    let mut errors = 0;
    loop {
        let env = fixture::next_frame_matching_on(client, expected_text, |env| {
            matches!(env.kind, FrameKind::AgentError | FrameKind::ChatEvent)
        })
        .await;
        if env.kind == FrameKind::AgentError {
            errors += 1;
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::StreamEnd(end) = event {
            assert!(
                end.message.content.contains(expected_text),
                "unexpected turn ended: {:?}",
                end.message.content
            );
            return errors;
        }
    }
}

async fn next_agent_error(client: &mut client::Connection, context: &str) -> AgentErrorPayload {
    fixture::next_frame_matching_on(client, context, |env| env.kind == FrameKind::AgentError)
        .await
        .parse_payload()
        .expect("parse AgentError")
}

/// A record torn by a full disk costs its own tail and nothing more.
///
/// `write_all` reports `ENOSPC` after committing part of a record, so the
/// journal ends in bytes that are not a record. Reading used to fail the whole
/// file over those bytes, which meant one bad write made the session
/// permanently unresumable — the history was still on disk, just unreachable.
/// Resume must still serve the intact prefix, and the damage must be cleared so
/// records written afterwards are readable too.
#[tokio::test]
async fn resume_serves_history_written_before_a_torn_transcript_record() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("torn".to_owned()),
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
        .await
        .expect("spawn agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);
    let intact_lines = journal_lines(&journal);
    assert!(
        !intact_lines.is_empty(),
        "the first turn should have written transcript records to {}",
        journal.display()
    );

    // Exactly what a full disk leaves behind: the leading bytes of a record,
    // with no terminator and no way to finish it. The session id inside it is a
    // sentinel, because every intact record opens with the same field and the
    // damage has to stay tellable apart from healthy bytes.
    let torn = br#"{"logical_session_id":"torn-by-a-full-disk","sequence":4"#;
    let torn_sentinel = b"torn-by-a-full-disk";
    let before_tear = std::fs::metadata(&journal).expect("stat journal").len();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal to tear it");
        file.write_all(torn).expect("write torn record");
    }

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");

    // The history written before the tear is what the tear must not cost.
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    // Appending behind torn bytes would bury every later record where no
    // reader can reach it, so the damage has to be gone.
    let contents = std::fs::read(&journal).expect("read journal after resume");
    assert!(
        !contents
            .windows(torn_sentinel.len())
            .any(|window| window == torn_sentinel),
        "the torn record should have been discarded from {}",
        journal.display()
    );
    assert_eq!(
        contents.last().copied(),
        Some(b'\n'),
        "every retained record must be terminated"
    );
    let repaired_lines = journal_lines(&journal);
    for line in &repaired_lines {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("every retained transcript record must parse");
    }
    // The repair may only ever remove from the end. Anything it rewrote or
    // dropped from the middle would be history lost to a fix meant to save it.
    assert_eq!(
        &repaired_lines[..intact_lines.len()],
        &intact_lines[..],
        "the records written before the tear must survive it unchanged"
    );
    assert!(
        repaired_lines.len() > intact_lines.len(),
        "records written after the repair should be readable: {} before, {} after",
        intact_lines.len(),
        repaired_lines.len()
    );
    assert!(
        std::fs::metadata(&journal)
            .expect("stat repaired journal")
            .len()
            >= before_tear,
        "the repair must keep the records that were already committed"
    );

    // A journal that parses but cannot be resumed from would be a repair in
    // name only, so read it back the way a user does: resume again and require
    // both turns, each exactly once.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed twice".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("second resume failed");

    let env = expect_next_event(&mut fixture.client, "second resume NewAgent").await;
    let again: NewAgentPayload = env.parse_payload().expect("parse second resume NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &again.instance_stream,
        FrameKind::AgentBootstrap,
        "second resume AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse second resume AgentBootstrap");
    let contents = bootstrap_message_contents(&payload);
    for turn in [
        "mock backend response to: hello",
        "mock backend response to: after resume",
    ] {
        let seen = contents
            .iter()
            .filter(|content| content.contains(turn))
            .count();
        assert_eq!(
            seen, 1,
            "expected {turn:?} exactly once after the repair, got {contents:?}"
        );
    }
    assert_eq!(
        contents.len(),
        2,
        "the repaired journal must replay both turns and nothing besides: {contents:?}"
    );
}

/// A transcript that stops being written says so.
///
/// The agent keeps running and the stream keeps updating when journaling
/// breaks, so the loss is invisible until a resume comes back short of the work
/// that was actually done. It has to reach the client while the turn is still
/// live, and it has to be non-fatal — the agent is fine, only its history is
/// not.
#[tokio::test]
async fn a_transcript_that_cannot_be_written_reports_a_non_fatal_error() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("unwritable".to_owned()),
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
        .await
        .expect("spawn agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);

    // A directory where the journal belongs fails every read and every write of
    // it, for any user, which is the durable stand-in for a disk that has
    // stopped accepting appends.
    // The actor may still be flushing the turn it just finished, so claim the
    // path until it stays claimed rather than racing it exactly once.
    let mut blocked = false;
    for _ in 0..100 {
        let _ = std::fs::remove_file(&journal);
        if std::fs::create_dir(&journal).is_ok() {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(blocked, "could not block {}", journal.display());

    fixture
        .client
        .send_message(
            &agent.instance_stream,
            "work that will not be saved".to_owned(),
        )
        .await
        .expect("send message failed");

    let error = next_agent_error(&mut fixture.client, "transcript persistence AgentError").await;
    assert_eq!(error.agent_id, agent.agent_id);
    assert!(
        !error.fatal,
        "a broken transcript must not be reported as a fatal agent error"
    );
    assert!(
        error.message.contains("transcript"),
        "the error should name what broke, got {:?}",
        error.message
    );

    // The agent itself is unaffected, which is precisely why the failure needs
    // announcing rather than inferring from the stream.
    let extra = expect_turn_counting_agent_errors(
        &mut fixture.client,
        "mock backend response to: work that will not be saved",
    )
    .await;
    assert_eq!(
        extra, 0,
        "one outage must report once, not once per event in the turn"
    );

    // A second turn in the same outage is still the same outage.
    fixture
        .client
        .send_message(&agent.instance_stream, "still not saved".to_owned())
        .await
        .expect("send second message failed");
    let extra = expect_turn_counting_agent_errors(
        &mut fixture.client,
        "mock backend response to: still not saved",
    )
    .await;
    assert_eq!(
        extra, 0,
        "a continuing outage must not re-report itself every turn"
    );
}

/// A record this build cannot read is not a record it may delete.
///
/// The repair exists for bytes no writer ever finished. A complete,
/// newline-terminated record that fails to deserialize is a different thing --
/// most likely written by a build that knows an event this one does not -- and
/// the journal is its only copy. Reading stops there, because guessing past it
/// would mis-sequence everything after, but the bytes have to stay on disk.
#[tokio::test]
async fn a_record_this_build_cannot_read_is_kept_on_disk() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("unknown schema".to_owned()),
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
        .await
        .expect("spawn agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    let session_id = wait_for_session_id(&mut fixture, "SessionList").await;
    let journal = transcript_journal_path(&fixture, &session_id);
    let intact_lines = journal_lines(&journal);

    // Well-formed JSON, properly terminated, carrying an event this build has
    // no variant for. A newer Tyde writing a newer event looks exactly so.
    let unknown = format!(
        r#"{{"logical_session_id":"{}","sequence":9999,"event_id":"from-a-newer-build","visibility":"visible","event":{{"type":"an_event_from_the_future","payload":{{"kept":true}}}},"timestamp_ms":1}}"#,
        session_id.0
    );
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("open journal");
        writeln!(file, "{unknown}").expect("write unknown record");
    }
    let after_write = std::fs::read_to_string(&journal).expect("read journal");

    // Resuming reads the journal, and reading is what used to truncate it.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("after resume".to_owned()),
            },
        })
        .await
        .expect("resume agent failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let env = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed AgentBootstrap",
    )
    .await;
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse resumed AgentBootstrap");

    // The prefix is still served: an unreadable record costs what follows it,
    // not what precedes it.
    assert_bootstrap_tail_messages(&payload, &["hello"]);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    let contents = std::fs::read_to_string(&journal).expect("read journal after resume");
    assert!(
        contents.contains("from-a-newer-build"),
        "a record this build cannot read must not be deleted from {}",
        journal.display()
    );
    assert!(
        contents.starts_with(&after_write),
        "the journal may only ever grow here, never be rewritten"
    );
    assert_eq!(
        journal_lines(&journal)[..intact_lines.len()],
        intact_lines[..],
        "records written before the unreadable one must be untouched"
    );
}

/// A backend-native child is a relay over its parent's sub-agent stream, not an
/// independently resumable session. Marking it for restoration made every
/// restart reconstruct it through the resume path, which rejects it and leaves
/// the user a failed card to dismiss. The parent still comes back; the child
/// must not come back at all.
#[tokio::test]
async fn restart_does_not_resurrect_backend_native_children() {
    let mut fixture = Fixture::new().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-with-native-child",
            MockScript::one(
                MockTurn::text("mock backend response to: parent prompt")
                    .with_native_child("mock-native-child", "parent prompt"),
            ),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent-with-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/restart-native-parent".to_owned()],
                prompt: "parent prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with native child");

    let parent: NewAgentPayload = expect_next_event(&mut fixture.client, "parent NewAgent")
        .await
        .parse_payload()
        .expect("parse parent NewAgent");
    let parent_start =
        expect_agent_start_on_stream(&mut fixture.client, &parent.instance_stream, "parent start")
            .await;
    let parent_session = parent_start.session_id.expect("parent session id");
    drop(reservation);
    expect_turn_on_stream(
        &mut fixture.client,
        &parent.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let child =
        fixture::next_frame_matching_on(&mut fixture.client, "native child NewAgent", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|agent| agent.parent_agent_id.as_ref() == Some(&parent.agent_id))
        })
        .await
        .parse_payload::<NewAgentPayload>()
        .expect("parse native child NewAgent");
    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "native child start",
    )
    .await;
    let child_session = child_start.session_id.expect("native child session id");
    assert_eq!(child_start.origin, protocol::AgentOrigin::BackendNative);
    assert_ne!(child_session, parent_session);
    // A relay child accepts no settings edits. Clients render its settings
    // from this snapshot, so without it they would read as loading forever.
    let child_settings = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "native child SessionSettings",
        |env| env.kind == FrameKind::SessionSettings && env.stream == child.instance_stream,
    )
    .await
    .parse_payload::<protocol::SessionSettingsPayload>()
    .expect("parse native child SessionSettings");
    assert!(child_settings.schema.is_none());

    let bootstrap = fixture.restart_host().await;
    let (mut restored, _) = collect_restart_replay(
        &mut fixture,
        &bootstrap,
        std::slice::from_ref(&parent_session),
    )
    .await;
    let restored_parent = restored.remove(&parent_session).expect("restored parent");
    assert_ne!(restored_parent.agent_id, parent.agent_id);

    // Restoration is one sequential pass, so the child would be reconstructed
    // moments after the parent. Give the pass room to do it and require that
    // the agent count never grows past the parent.
    let resurrected = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture.agent_ids().await.len() > 1 {
                return fixture.agent_session_ids().await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        resurrected.is_err(),
        "backend-native child must not be reconstructed after restart, live sessions: {:?}",
        resurrected.unwrap_or_default()
    );
    let live_sessions = fixture.agent_session_ids().await;
    assert_eq!(
        live_sessions,
        vec![parent_session.clone()],
        "only the parent session should be live after restart"
    );
    assert!(
        !live_sessions.contains(&child_session),
        "backend-native child session must not be reconstructed after restart"
    );
}

#[tokio::test]
async fn session_sqlite_import_preserves_json_and_survives_restart() {
    let mut original = Fixture::new().await;
    original
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Pre-migration conversation".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "before migration".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        })
        .await
        .expect("create provider session before import");
    let agent: NewAgentPayload = expect_next_event(&mut original.client, "pre-import NewAgent")
        .await
        .parse_payload()
        .expect("parse original agent");
    let start = expect_agent_start_on_stream(
        &mut original.client,
        &agent.instance_stream,
        "pre-import AgentStart",
    )
    .await;
    let session_id = start.session_id.expect("original provider session");
    expect_turn_on_stream(
        &mut original.client,
        &agent.instance_stream,
        "mock backend response to: before migration",
    )
    .await;
    original
        .client
        .close_agent(&agent.instance_stream)
        .await
        .expect("close original session");
    fixture::next_frame_matching_on(&mut original.client, "original session closed", |env| {
        env.kind == FrameKind::AgentClosed
    })
    .await;

    let sessions = r#"{"write_seq":9,"records":{"imported":{"id":"imported","backend_kind":"claude","workspace_roots":["/tmp/test"],"alias":"Imported conversation","user_alias":"My saved name","created_at_ms":1,"updated_at_ms":2,"message_count":7,"token_count":123,"access_mode":"read_only","queued_messages":[],"future_metadata":{"preserve":true}}}}"#;
    let tasks = r#"{"records":{"imported":{"title":"Saved tasks","tasks":[{"id":1,"description":"Keep this task","status":"pending"}]}}}"#;
    let sessions = sessions.replace("imported", &session_id.0);
    let tasks = tasks.replace("imported", &session_id.0);
    let mut fixture = Fixture::new_with_session_import(&sessions, &tasks).await;
    let legacy = fixture.store_dir().join("sessions.json");
    let database = SessionStore::database_path(&legacy);
    assert!(
        database.is_file(),
        "startup must create the session database"
    );
    assert!(
        std::fs::read_to_string(&legacy).expect("read preserved JSON") == sessions,
        "session source must remain byte-for-byte unchanged"
    );
    assert!(
        std::fs::read_to_string(legacy.with_extension("task-lists.json"))
            .expect("read preserved task JSON")
            == tasks,
        "task source must remain byte-for-byte unchanged"
    );
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list imported sessions");
    let list = wait_for_session_list(&mut fixture.client, "imported sessions").await;
    assert_eq!(list.sessions.len(), 1);
    let imported = &list.sessions[0];
    assert!(
        imported.id == session_id,
        "imported session identity must survive"
    );
    assert!(
        imported.user_alias.as_deref() == Some("My saved name"),
        "imported alias must survive"
    );
    assert_eq!(imported.message_count, 7);
    assert_eq!(imported.token_count, Some(123));

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume imported session");
    let env = expect_next_event(&mut fixture.client, "imported NewAgent").await;
    let agent: NewAgentPayload = env.parse_payload().expect("parse imported agent");
    let task_event = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &agent.instance_stream,
        "imported tasks",
        |event| matches!(event, ChatEvent::TaskUpdate(_)),
    )
    .await;
    assert!(
        matches!(task_event, ChatEvent::TaskUpdate(list) if list.title == "Saved tasks" && list.tasks.len() == 1 && list.tasks[0].description == "Keep this task"),
        "resume must expose imported tasks"
    );

    fixture
        .client
        .set_agent_name(&agent.instance_stream, "Changed in SQLite".to_owned())
        .await
        .expect("rename imported session");
    fixture::next_frame_matching_on(&mut fixture.client, "persisted rename", |env| {
        env.kind == FrameKind::AgentRenamed
    })
    .await;
    fixture
        .client
        .close_agent(&agent.instance_stream)
        .await
        .expect("close imported session");
    fixture::next_frame_matching_on(&mut fixture.client, "closed imported session", |env| {
        env.kind == FrameKind::AgentClosed
    })
    .await;
    fixture.restart_host().await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list after restart");
    let list = wait_for_session_list(&mut fixture.client, "restarted imported sessions").await;
    assert!(
        list.sessions
            .iter()
            .any(|session| session.user_alias.as_deref() == Some("Changed in SQLite")),
        "restart must not reimport stale JSON over committed state"
    );
    let connection = rusqlite::Connection::open(&database).expect("inspect imported database");
    let json: String = connection
        .query_row(
            "SELECT record FROM sessions WHERE id=?1",
            [&session_id.0],
            |row| row.get(0),
        )
        .expect("read imported row");
    let json: serde_json::Value = serde_json::from_str(&json).expect("decode imported row");
    assert_eq!(
        json["future_metadata"]["preserve"], true,
        "unknown metadata must survive subsequent writes"
    );
    assert!(
        std::fs::read_to_string(&legacy).expect("read preserved JSON after restart") == sessions,
        "runtime writes must not modify the legacy backup"
    );
}

#[tokio::test]
async fn session_reads_remain_responsive_during_uncommitted_write() {
    let mut fixture = Fixture::new_with_store_files(
        r#"{"records":{"saved":{"id":"saved","backend_kind":"claude","workspace_roots":["/tmp/test"],"created_at_ms":1,"updated_at_ms":1}}}"#,
        r#"{"version":2,"records":{}}"#,
    ).await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Before commit".to_owned()),
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
        .await
        .expect("spawn persistence test agent");
    let env = expect_next_event(&mut fixture.client, "persistence NewAgent").await;
    let agent: NewAgentPayload = env.parse_payload().expect("parse persistence agent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "persistence start",
    )
    .await;
    let session = start.session_id.expect("session identity present");
    expect_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "mock backend response to: hello",
    )
    .await;
    // The writer is a host-level deletion, not a command on the live agent:
    // bootstrap also asks live actors for usage, independently of session I/O.
    let mut deleting_client = fixture.connect().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let hook = fixture.on_next_session_commit(move || {
        let _ = entered_tx.send(());
        let _ = release_rx.recv_timeout(Duration::from_secs(15));
    });
    deleting_client
        .delete_session(DeleteSessionPayload {
            session_id: SessionId("saved".to_owned()),
        })
        .await
        .expect("submit saved-session deletion");
    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .expect("writer must enter transaction")
        .expect("commit hook signal");

    let mut observer = tokio::time::timeout(Duration::from_secs(2), fixture.connect())
        .await
        .expect("host bootstrap must not wait for session commit");
    observer
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list while commit is paused");
    let list = tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_session_list(&mut observer, "list during stalled commit"),
    )
    .await
    .expect("session read must not wait for writer");
    assert!(
        list.sessions.iter().any(|record| record.id == session),
        "unrelated live session remains visible"
    );
    assert!(
        list.sessions.iter().any(|record| record.id.0 == "saved"),
        "read must not expose an uncommitted deletion"
    );
    assert_eq!(list.sessions.len(), 2);
    release_tx.send(()).expect("release session commit");
    drop(hook);
    deleting_client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list committed deletion");
    let list = wait_for_session_list(&mut deleting_client, "list after commit").await;
    assert_eq!(list.sessions.len(), 1);
    assert!(
        list.sessions[0].id == session,
        "only the unrelated session remains"
    );
    fixture.restart_host().await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list recovered deletion");
    let list = wait_for_session_list(&mut fixture.client, "list recovered commit").await;
    assert_eq!(list.sessions.len(), 1);
    assert!(
        list.sessions[0].id == session,
        "restart must not resurrect a deleted session from legacy JSON"
    );
}

#[tokio::test]
async fn session_import_failure_rolls_back_and_retries_without_data_loss() {
    let directory = tempfile::tempdir().expect("create import directory");
    let legacy = directory.path().join("sessions.json");
    let tasks = legacy.with_extension("task-lists.json");
    let source = r#"{"records":{"retained":{"id":"retained","backend_kind":"claude","workspace_roots":["/tmp/test"],"created_at_ms":1,"updated_at_ms":1}}}"#;
    std::fs::write(&legacy, source).expect("seed session import");
    std::fs::write(&tasks, r#"{"records":{"retained":{"title":42}}}"#)
        .expect("seed invalid task import");
    let start = || {
        server::spawn_host_with_mock_backend_and_runtime_config(
            legacy.clone(),
            directory.path().join("projects.json"),
            directory.path().join("settings.json"),
            server::HostRuntimeConfig {
                skip_real_backend_probe: true,
                ..Default::default()
            },
        )
    };
    assert!(
        start().is_err(),
        "invalid task import must fail server startup"
    );
    assert!(
        std::fs::read_to_string(&legacy).expect("read original import") == source,
        "failed import must not modify original sessions"
    );
    let database = SessionStore::database_path(&legacy);
    let connection = rusqlite::Connection::open(&database).expect("inspect failed import");
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read import marker");
    assert_eq!(version, 0, "failed import must not mark migration complete");
    let tables: u32 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('sessions','session_tasks','archived_sessions')", [], |row| row.get(0)).expect("inspect rollback");
    assert_eq!(
        tables, 0,
        "failed import must roll back schema and imported rows together"
    );
    drop(connection);
    std::fs::write(
        &tasks,
        r#"{"records":{"retained":{"title":"Recovered tasks","tasks":[]}}}"#,
    )
    .expect("repair task import");
    let host = start().expect("retry complete import");
    let mut client = fixture::connect_client(host).await;
    client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list retried import");
    let list = wait_for_session_list(&mut client, "retried import").await;
    assert_eq!(list.sessions.len(), 1);
    assert!(
        list.sessions[0].id.0 == "retained",
        "retry must retain the original session"
    );
}
