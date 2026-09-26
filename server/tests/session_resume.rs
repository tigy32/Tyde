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
use server::backend::mock::{AbandonedResponse, MockScript, MockTurn, RunningCommandShape};
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
                    | FrameKind::AgentActivityChanged
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
                | FrameKind::AgentActivityChanged
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
                | FrameKind::AgentActivityChanged
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
        assert_eq!(
            restored.activity,
            protocol::AgentActivity::Idle,
            "replay alone is not a live turn"
        );
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

        assert_eq!(
            bootstrap.activity,
            protocol::AgentActivity::Thinking,
            "eager bootstrap must include the live start received before the replay boundary"
        );
        assert!(
            bootstrap.events.iter().any(|event| matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(true))
            )),
            "pre-boundary live typing must reach the reducer before bootstrap"
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
        assert_eq!(
            state.activity,
            protocol::AgentActivity::Thinking,
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
        assert_eq!(
            late.activity,
            protocol::AgentActivity::Thinking,
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
        assert_eq!(
            late_bootstrap.activity,
            protocol::AgentActivity::Thinking,
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
        assert_eq!(state.activity, protocol::AgentActivity::Idle);
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
async fn resume_completed_turn_and_followup_settle_before_attach() {
    for (follow_up, ongoing, start_before_boundary) in [
        (false, false, true),
        (true, false, true),
        (true, true, true),
        (true, true, false),
    ] {
        let mut fixture = Fixture::new().await;
        let replay = server::backend::mock::MockResumeReplay::default();
        let source = fixture
            .spawn_scripted(
                "completed before boundary source",
                MockScript::one(MockTurn::text("saved history"))
                    .with_controlled_resume_replay(&replay),
            )
            .await;
        fixture.finish_turn(&source).await;
        let session_id = fixture.agent_session_ids().await.remove(0);
        let finish = server::backend::mock::MockGateHandle::new();
        let reservation = fixture
            .reserve_next_mock_launch(
                "completed before boundary",
                MockScript::one(MockTurn::text_after_gate("follow-up response", &finish)),
            )
            .await;
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                name: Some("completed before boundary".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id,
                    prompt: follow_up.then(|| "resume follow-up".to_owned()),
                },
            })
            .await
            .expect("resume with controlled boundary");
        let agent = expect_next_event(&mut fixture.client, "resumed NewAgent")
            .await
            .parse_payload::<NewAgentPayload>()
            .expect("resumed descriptor");
        replay.wait_until_started().await;
        if !start_before_boundary {
            replay.complete();
        }
        if ongoing {
            replay.start_live_turn("provider resumed response");
        } else {
            replay.live_turn("provider resumed response");
        }
        if start_before_boundary {
            replay.complete();
        }
        let bootstrap = expect_raw_event_on_stream(
            &mut fixture.client,
            &agent.instance_stream,
            FrameKind::AgentBootstrap,
            "completed pre-boundary turn bootstrap",
        )
        .await
        .parse_payload::<AgentBootstrapPayload>()
        .expect("resumed bootstrap");
        if !ongoing {
            assert!(
                bootstrap_message_contents(&bootstrap)
                    .contains(&"provider resumed response".to_owned()),
                "a live turn completed before the boundary must not be dropped as provider history"
            );
        }
        assert_eq!(
            bootstrap.activity == protocol::AgentActivity::Thinking,
            follow_up,
            "first bootstrap must reflect accepted or busy follow-up dispatch"
        );
        if ongoing {
            assert!(
                bootstrap.events.iter().any(|event| matches!(
                    event,
                    AgentBootstrapEvent::QueuedMessages(payload) if payload.messages.len() == 1
                )),
                "the Busy initial follow-up must remain queued"
            );
            if !start_before_boundary {
                for expected in ["typing", "start", "delta"] {
                    let event = expect_chat_event_on_stream(
                        &mut fixture.client,
                        &agent.instance_stream,
                        "post-boundary provider start",
                    )
                    .await;
                    assert!(matches!(
                        (expected, event),
                        ("typing", ChatEvent::TypingStatusChanged(true))
                            | ("start", ChatEvent::StreamStart(_))
                            | ("delta", ChatEvent::StreamDelta(_))
                    ));
                }
            }
            replay.finish_live_turn("provider resumed response");
            let event = expect_chat_event_on_stream(
                &mut fixture.client,
                &agent.instance_stream,
                "provider end after Busy",
            )
            .await;
            assert!(matches!(event, ChatEvent::StreamEnd(_)));
            let event = expect_chat_event_on_stream(
                &mut fixture.client,
                &agent.instance_stream,
                "armed provider completion after Busy",
            )
            .await;
            assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
            let queue = fixture::next_frame_matching_on(
                &mut fixture.client,
                "armed completion dispatches the queued follow-up",
                |env| env.stream == agent.instance_stream && env.kind == FrameKind::QueuedMessages,
            )
            .await
            .parse_payload::<protocol::QueuedMessagesPayload>()
            .expect("queued follow-up state");
            assert!(queue.messages.is_empty(), "follow-up must leave the queue");
        }
        if follow_up {
            finish.wait_until_entered().await;
            finish.release_one();
            expect_turn_on_stream(
                &mut fixture.client,
                &agent.instance_stream,
                "follow-up response",
            )
            .await;
        }
        let control = fixture.connect_agent_control().await;
        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            control.await_agents(Some(vec![agent.agent_id.clone()])),
        )
        .await
        .expect("completed resume and follow-up settle")
        .expect("agent-control await succeeds");
        assert_eq!(settled.ready.len(), 1);
        assert_eq!(settled.ready[0].status, protocol::AgentControlStatus::Idle);
        assert!(settled.still_thinking.is_empty());
        drop(reservation);
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
    // Restoration keeps each agent's persisted id, so an orchestrator that
    // held a child id before the restart still addresses the same child.
    assert_eq!(
        restored.agent_id, survivor.agent_id,
        "restart must restore the parent under its persisted agent id"
    );
    assert_eq!(
        restored_child.agent_id, survivor_child.agent_id,
        "restart must restore the child under its persisted agent id"
    );
    assert_eq!(restored_child.name, "child survives restart");
    // The durable parent session id rebuilds the lineage; the restored child
    // must hang off the restored parent's agent id.
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
        assert_eq!(
            bootstrap.activity,
            protocol::AgentActivity::Idle,
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
    // A completed bootstrap reports activity=Idle instead of replaying a
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
    assert_eq!(
        restored_parent.agent_id, parent.agent_id,
        "restart must restore the parent under its persisted agent id"
    );

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

fn restart_expiry_of(
    events: &[AgentBootstrapEvent],
    tool_call_id: &str,
) -> Vec<protocol::ToolExecutionOutcome> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
                if completion.tool_call_id == tool_call_id =>
            {
                Some(completion.outcome.clone())
            }
            _ => None,
        })
        .collect()
}

/// A question or plan approval left pending when the app was killed, or when
/// it restarted gracefully, replays on relaunch, but the relaunched backend
/// holds nothing that could accept an answer. The server must close the card
/// with a typed expiry, persist it, and keep refusing answers to it — never
/// leave it answerable, nor report it as a tool whose outcome is unknown.
#[tokio::test]
async fn relaunch_after_kill_expires_unanswered_questions_and_plans() {
    for (plan, graceful) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut fixture = Fixture::new().await;
        let gate = server::backend::mock::MockGateHandle::new();
        let (tool_call_id, tool_name, script) = if plan {
            (
                "orphaned-plan",
                "ExitPlanMode",
                MockScript::one(MockTurn::text("launch response"))
                    .then(MockTurn::exit_plan_request("orphaned-plan", "# Plan")),
            )
        } else {
            (
                "orphaned-question",
                "AskUserQuestion",
                MockScript::one(MockTurn::text("launch response")).then(
                    MockTurn::blocking_question_request("orphaned-question", &gate),
                ),
            )
        };
        let agent = fixture.spawn_scripted("orphaned interaction", script).await;
        // The kill must come after a completed turn: that is what makes the
        // server's transcript, not the backend, the replayed history.
        fixture.finish_turn(&agent).await;
        fixture
            .client
            .send_message(&agent.stream, "ask me".to_owned())
            .await
            .expect("send the turn that asks");
        if !plan {
            gate.wait_until_entered().await;
            gate.release_one();
        }
        let request = fixture.expect_paused_tool_request(&agent, tool_name).await;
        assert_eq!(request.tool_call_id, tool_call_id);
        // A mailbox round trip finishes persisting the request before the kill.
        fixture.mock(&agent).await;
        let session_id = fixture
            .agent_session_ids()
            .await
            .into_iter()
            .next()
            .expect("orphaned interaction session");

        let mut restored_stream = None;
        for relaunch in 1..=2 {
            let relaunched = if graceful {
                fixture.restart_host().await
            } else {
                fixture.relaunch_host_after_kill().await
            };
            let (agents, bootstraps) = collect_restart_replay(
                &mut fixture,
                &relaunched,
                std::slice::from_ref(&session_id),
            )
            .await;
            let restored = agents.get(&session_id).expect("restored agent");
            let bootstrap = &bootstraps[&restored.instance_stream];
            assert!(
                bootstrap.events.iter().any(|event| matches!(
                    event,
                    AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(replayed))
                        if replayed.tool_call_id == tool_call_id
                )),
                "{tool_name} graceful={graceful} relaunch {relaunch}: the card must still replay"
            );
            let mut outcomes = restart_expiry_of(&bootstrap.events, tool_call_id);
            if outcomes.is_empty() {
                let event = fixture::next_frame_matching_on(
                    &mut fixture.client,
                    "expiry of the orphaned interaction",
                    |env| {
                        env.stream == restored.instance_stream
                            && env.kind == FrameKind::ChatEvent
                            && matches!(
                                env.parse_payload::<ChatEvent>(),
                                Ok(ChatEvent::ToolExecutionCompleted(completion))
                                    if completion.tool_call_id == tool_call_id
                            )
                    },
                )
                .await
                .parse_payload::<ChatEvent>()
                .expect("parse expiry");
                let ChatEvent::ToolExecutionCompleted(completion) = event else {
                    unreachable!("matched a completion");
                };
                outcomes.push(completion.outcome);
            }
            assert_eq!(
                outcomes.len(),
                1,
                "{tool_name} graceful={graceful} relaunch {relaunch}: exactly one terminal outcome, got {outcomes:?}"
            );
            assert!(
                matches!(
                    &outcomes[0],
                    protocol::ToolExecutionOutcome::Cancelled { message }
                        if message.contains("can no longer be answered")
                ),
                "{tool_name} graceful={graceful} relaunch {relaunch}: the card must expire, got {:?}",
                outcomes[0]
            );
            restored_stream = Some(restored.instance_stream.clone());
        }

        let tool_response = if plan {
            protocol::SendMessageToolResponse::ExitPlanMode {
                tool_call_id: tool_call_id.to_owned(),
                decision: protocol::ExitPlanModeDecision::Approve,
                feedback: None,
            }
        } else {
            protocol::SendMessageToolResponse::AskUserQuestion {
                tool_call_id: tool_call_id.to_owned(),
                answer: "Rust".to_owned(),
            }
        };
        let restored_stream = restored_stream.expect("relaunched agent stream");
        fixture
            .client
            .send_message_payload(
                &restored_stream,
                protocol::SendMessagePayload {
                    message: "Rust".to_owned(),
                    images: None,
                    origin: None,
                    tool_response: Some(tool_response),
                },
            )
            .await
            .expect("send answer to the expired interaction");
        let refused = fixture::next_frame_matching_on(
            &mut fixture.client,
            "refusal of an answer to the expired interaction",
            |env| {
                env.stream == restored_stream
                    && env.kind == FrameKind::ChatEvent
                    && matches!(
                        env.parse_payload::<ChatEvent>(),
                        Ok(ChatEvent::MessageAdded(_) | ChatEvent::ToolExecutionCompleted(_))
                    )
            },
        )
        .await
        .parse_payload::<ChatEvent>()
        .expect("parse refusal");
        assert!(
            matches!(
                &refused,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, protocol::MessageSender::Error)
                        && message.content.contains("No matching pending tool request")
            ),
            "{tool_name}: an answer to an expired card must be refused, got {refused:?}"
        );
    }
}

/// Spawn a scripted agent and return the session the server persisted for it.
async fn spawn_restart_agent(
    fixture: &mut Fixture,
    name: &str,
    prompt: &str,
    parent_agent_id: Option<protocol::AgentId>,
    script: MockScript,
) -> (fixture::TestAgent, SessionId) {
    let reservation = fixture.reserve_next_mock_launch(name, script).await;
    let (agent, start) = fixture
        .spawn_with(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![format!("/tmp/{}", name.replace(' ', "-"))],
                prompt: prompt.to_owned(),
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

/// Restored `NewAgent` descriptors and `AgentBootstrap` payloads observed so
/// far after a restart. Restoration interleaves both frame kinds across
/// agents, so a wait for one agent's descriptor must retain another agent's
/// bootstrap rather than discarding it.
#[derive(Default)]
struct RestoredReplay {
    agents: std::collections::HashMap<SessionId, NewAgentPayload>,
    bootstraps: std::collections::HashMap<StreamPath, AgentBootstrapPayload>,
}

impl RestoredReplay {
    fn seed(bootstrap: &settings_model::HostBootstrapPayload) -> Self {
        let mut replay = Self::default();
        for agent in &bootstrap.agents {
            if let Some(session_id) = agent.session_id.as_ref() {
                replay.agents.insert(session_id.clone(), agent.clone());
            }
        }
        replay
    }

    async fn wait_until(
        &mut self,
        fixture: &mut Fixture,
        context: &str,
        mut done: impl FnMut(&Self) -> bool,
    ) {
        while !done(self) {
            let env = fixture::next_frame_matching_on(&mut fixture.client, context, |env| {
                matches!(env.kind, FrameKind::NewAgent | FrameKind::AgentBootstrap)
            })
            .await;
            match env.kind {
                FrameKind::NewAgent => {
                    let agent: NewAgentPayload =
                        env.parse_payload().expect("parse restored NewAgent");
                    if let Some(session_id) = agent.session_id.as_ref() {
                        self.agents.insert(session_id.clone(), agent);
                    }
                }
                FrameKind::AgentBootstrap => {
                    let payload: AgentBootstrapPayload =
                        env.parse_payload().expect("parse restored AgentBootstrap");
                    self.bootstraps.insert(env.stream.clone(), payload);
                }
                kind => unreachable!("unexpected restart replay frame {kind:?}"),
            }
        }
    }

    async fn agents(
        &mut self,
        fixture: &mut Fixture,
        sessions: &[SessionId],
    ) -> Vec<NewAgentPayload> {
        self.wait_until(fixture, "restored NewAgent", |replay| {
            sessions
                .iter()
                .all(|session| replay.agents.contains_key(session))
        })
        .await;
        sessions
            .iter()
            .map(|session| self.agents[session].clone())
            .collect()
    }

    async fn bootstrap(
        &mut self,
        fixture: &mut Fixture,
        stream: &StreamPath,
    ) -> AgentBootstrapPayload {
        self.wait_until(fixture, "restored AgentBootstrap", |replay| {
            replay.bootstraps.contains_key(stream)
        })
        .await;
        self.bootstraps[stream].clone()
    }
}

fn recovery_phases(events: &[AgentBootstrapEvent]) -> Vec<protocol::RestartRecoveryPhase> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::RestartRecovery { phase }) => {
                Some(phase.clone())
            }
            _ => None,
        })
        .collect()
}

/// Every message the mock backend was asked to run, in arrival order.
async fn mock_inputs(
    fixture: &Fixture,
    agent_id: &protocol::AgentId,
) -> Vec<protocol::SendMessagePayload> {
    fixture
        .mock_by_id(agent_id)
        .await
        .requests()
        .await
        .into_iter()
        .filter_map(|request| match request {
            server::backend::mock::MockRequest::Input(input) => Some(input),
            _ => None,
        })
        .collect()
}

fn stored_turn_recovery(fixture: &Fixture, session_id: &SessionId) -> serde_json::Value {
    let store = SessionStore::load(fixture.session_store_path()).expect("load session store");
    let record = store.get(session_id).expect("stored session record");
    serde_json::to_value(&record).expect("inspect stored record")["turn_recovery"].clone()
}

/// One thing a client saw on a restored agent's stream, in arrival order,
/// whether it came inside the agent's bootstrap or as a live frame.
#[derive(Debug, Clone, PartialEq)]
enum Observed {
    Bootstrap {
        activity: protocol::AgentActivity,
        phases: Vec<protocol::RestartRecoveryPhase>,
    },
    Phase(protocol::RestartRecoveryPhase),
    Delta(String),
    Queue(usize),
    Typing(bool),
}

#[derive(Default)]
struct StreamObservation {
    items: Vec<Observed>,
}

impl StreamObservation {
    fn record_chat_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::RestartRecovery { phase } => self.items.push(Observed::Phase(phase)),
            ChatEvent::StreamDelta(delta) => self.items.push(Observed::Delta(delta.text)),
            ChatEvent::TypingStatusChanged(active) => self.items.push(Observed::Typing(active)),
            _ => {}
        }
    }

    /// Recovery phases in the order the client learned them: those carried by
    /// the bootstrap first, then any streamed live.
    fn phases(&self) -> Vec<protocol::RestartRecoveryPhase> {
        self.items
            .iter()
            .flat_map(|item| match item {
                Observed::Bootstrap { phases, .. } => phases.clone(),
                Observed::Phase(phase) => vec![phase.clone()],
                _ => Vec::new(),
            })
            .collect()
    }

    fn deltas(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| match item {
                Observed::Delta(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn queue_counts(&self) -> Vec<usize> {
        self.items
            .iter()
            .filter_map(|item| match item {
                Observed::Queue(count) => Some(*count),
                _ => None,
            })
            .collect()
    }

    fn bootstrap(&self) -> Option<&Observed> {
        self.items
            .iter()
            .find(|item| matches!(item, Observed::Bootstrap { .. }))
    }

    /// The restart continuation ran to idle, whether the client saw it stream
    /// or attached only after it finished. An agent's attach waits for its
    /// startup, so under load its bootstrap can already report the finished
    /// continuation.
    fn continuation_finished(&self) -> bool {
        self.settled_after(CONTINUATION_PREFIX)
            || matches!(
                self.bootstrap(),
                Some(Observed::Bootstrap {
                    activity: protocol::AgentActivity::Idle,
                    phases,
                }) if phases.contains(&protocol::RestartRecoveryPhase::Continuing)
            )
    }

    /// The stream went idle after streaming text containing `needle`.
    fn settled_after(&self, needle: &str) -> bool {
        let Some(delta) = self
            .items
            .iter()
            .position(|item| matches!(item, Observed::Delta(text) if text.contains(needle)))
        else {
            return false;
        };
        self.items[delta..].contains(&Observed::Typing(false))
    }
}

/// Everything the client saw on `streams`, per stream and as one global
/// sequence, so a test can assert on ordering across restored agents.
#[derive(Default)]
struct Observation {
    streams: std::collections::HashMap<StreamPath, StreamObservation>,
    sequence: Vec<(StreamPath, Observed)>,
}

impl Observation {
    fn record(&mut self, stream: &StreamPath, item: Observed) {
        self.streams
            .entry(stream.clone())
            .or_default()
            .items
            .push(item.clone());
        self.sequence.push((stream.clone(), item));
    }

    fn stream(&self, stream: &StreamPath) -> &StreamObservation {
        self.streams
            .get(stream)
            .unwrap_or_else(|| panic!("nothing observed on {stream}"))
    }

    /// Index in the global sequence at which the client first learned `phase`
    /// for `stream`, whether inside its bootstrap or as a live event.
    fn phase_position(&self, stream: &StreamPath, phase: &protocol::RestartRecoveryPhase) -> usize {
        self.sequence
            .iter()
            .position(|(observed_stream, observed)| {
                observed_stream == stream
                    && match observed {
                        Observed::Bootstrap { phases, .. } => phases.contains(phase),
                        Observed::Phase(observed) => observed == phase,
                        _ => false,
                    }
            })
            .unwrap_or_else(|| panic!("{phase:?} never observed on {stream}: {:?}", self.sequence))
    }

    /// Read frames on `streams` until `done`, keeping every frame on those
    /// streams. Frames on other streams are not needed by a restart test once
    /// its restored descriptors are known.
    async fn observe_until(
        fixture: &mut Fixture,
        streams: &[StreamPath],
        done: impl FnMut(&Self) -> bool,
    ) -> Self {
        let mut observation = Self::default();
        for stream in streams {
            observation.streams.entry(stream.clone()).or_default();
        }
        Self::continue_until(observation, fixture, streams, done).await
    }

    /// Keep reading into this observation until `done`.
    async fn continue_until(
        mut observation: Self,
        fixture: &mut Fixture,
        streams: &[StreamPath],
        mut done: impl FnMut(&Self) -> bool,
    ) -> Self {
        while !done(&observation) {
            let env = match tokio::time::timeout(
                Duration::from_secs(5),
                fixture::next_frame_matching_on_with_timeout(
                    &mut fixture.client,
                    "restored stream activity",
                    Duration::from_secs(10),
                    |env| streams.contains(&env.stream),
                ),
            )
            .await
            {
                Ok(env) => env,
                Err(_) => panic!(
                    "timed out waiting for restored stream activity; observed so far: {:#?}",
                    observation.sequence
                ),
            };
            match env.kind {
                FrameKind::AgentBootstrap => {
                    let payload: AgentBootstrapPayload =
                        env.parse_payload().expect("parse restored AgentBootstrap");
                    observation.record(
                        &env.stream,
                        Observed::Bootstrap {
                            activity: payload.activity,
                            phases: recovery_phases(&payload.events),
                        },
                    );
                    for event in payload.events {
                        if let AgentBootstrapEvent::QueuedMessages(payload) = event {
                            observation
                                .record(&env.stream, Observed::Queue(payload.messages.len()));
                        }
                    }
                }
                FrameKind::QueuedMessages => {
                    let payload: protocol::QueuedMessagesPayload =
                        env.parse_payload().expect("parse QueuedMessages");
                    observation.record(&env.stream, Observed::Queue(payload.messages.len()));
                }
                FrameKind::ChatEvent => {
                    let mut single = StreamObservation::default();
                    single.record_chat_event(env.parse_payload().expect("parse ChatEvent"));
                    for item in single.items {
                        observation.record(&env.stream, item);
                    }
                }
                _ => {}
            }
        }
        observation
    }
}

/// Asserts the provider accepted `child`'s restart continuation before
/// `parent`'s. Bootstraps arrive whenever each attach is served, so their
/// order across agents is not the order the continuations were sent in.
fn assert_continued_before(child: &SessionId, parent: &SessionId) {
    let accepted = server::backend::mock::accepted_messages_in_order();
    let position = |session: &SessionId| {
        accepted
            .iter()
            .position(|(accepted_session, message)| {
                accepted_session == session
                    && message.origin == Some(protocol::MessageOrigin::HostRestart)
            })
            .unwrap_or_else(|| {
                panic!(
                    "no restart continuation was accepted for a restored agent; accepted origins: {:?}",
                    accepted
                        .iter()
                        .map(|(_, message)| message.origin.clone())
                        .collect::<Vec<_>>()
                )
            })
    };
    assert!(
        position(child) < position(parent),
        "the child's continuation must reach the provider before its parent's"
    );
}

fn delta_position(deltas: &[String], needle: &str) -> usize {
    deltas
        .iter()
        .position(|text| text.contains(needle))
        .unwrap_or_else(|| panic!("no streamed text contained {needle:?}: {deltas:?}"))
}

const CONTINUATION_PREFIX: &str = "Tyde restarted while you were working";

/// A turn that was running when the host restarts, cleanly or by a hard
/// kill, comes back on the same agent id, explains the interruption, and is
/// continued by the host as a live turn ahead of the messages the user had
/// queued, which then run in their original order. An agent that was idle at
/// the restart is reopened untouched.
#[tokio::test]
async fn interrupted_turns_continue_after_graceful_and_hard_restart() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    for hard in [false, true] {
        let mut fixture = Fixture::new().await;
        let (bystander, bystander_session) = spawn_restart_agent(
            &mut fixture,
            "idle bystander",
            "bystander prompt",
            None,
            MockScript::one(MockTurn::text("bystander done")),
        )
        .await;
        fixture.finish_turn(&bystander).await;
        let replay = server::backend::mock::MockResumeReplay::default();
        let (agent, session) = spawn_restart_agent(
            &mut fixture,
            "restart continuation",
            "unfinished prompt",
            None,
            MockScript::one(MockTurn::held_text("unfinished"))
                .with_controlled_resume_replay(&replay),
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
        let mut ids_before = fixture.agent_ids().await;
        ids_before.sort_by(|a, b| a.0.cmp(&b.0));

        let bootstrap = if hard {
            fixture.relaunch_host_after_kill().await
        } else {
            fixture.restart_host().await
        };
        let mut restored = RestoredReplay::seed(&bootstrap);
        let descriptors = restored
            .agents(&mut fixture, &[session.clone(), bystander_session.clone()])
            .await;
        let (held, idle) = (&descriptors[0], &descriptors[1]);
        assert_eq!(
            held.agent_id, agent.new_agent.agent_id,
            "hard={hard}: restoration must keep the persisted agent identity"
        );
        assert_eq!(idle.agent_id, bystander.new_agent.agent_id);

        // The idle agent's bootstrap is not gated on anything this test holds,
        // so take it before releasing the interrupted agent's replay: that
        // replay is held until the restart descriptor has reached this client,
        // so its bootstrap is built at the replay boundary and shows exactly
        // what the host decided there.
        let idle_bootstrap = restored
            .bootstrap(&mut fixture, &idle.instance_stream)
            .await;
        assert!(
            recovery_phases(&idle_bootstrap.events).is_empty(),
            "hard={hard}: an idle agent has nothing to recover: {:?}",
            idle_bootstrap.events
        );
        assert!(
            idle_bootstrap.activity == protocol::AgentActivity::Idle,
            "hard={hard}: an idle agent must reopen idle"
        );
        replay.wait_until_started().await;
        replay.complete();
        let observation = Observation::observe_until(
            &mut fixture,
            std::slice::from_ref(&held.instance_stream),
            |observation| {
                observation
                    .stream(&held.instance_stream)
                    .settled_after("queued second")
            },
        )
        .await;
        let flow = observation.stream(&held.instance_stream);
        let expected_cause = if hard {
            Cause::UnexpectedStop
        } else {
            Cause::HostRestart
        };
        let Some(Observed::Bootstrap {
            activity,
            phases: bootstrap_phases,
        }) = flow.bootstrap()
        else {
            panic!(
                "hard={hard}: the restored agent must be bootstrapped: {:?}",
                flow.items
            );
        };
        assert_eq!(
            *activity,
            protocol::AgentActivity::Thinking,
            "hard={hard}: an interrupted turn awaiting its continuation reopens live, not idle"
        );
        assert_eq!(
            bootstrap_phases.first(),
            Some(&Phase::Interrupted {
                cause: expected_cause
            }),
            "hard={hard}: the bootstrap explains the interruption before anything else: {:?}",
            flow.items
        );
        assert_eq!(
            flow.phases(),
            vec![
                Phase::Interrupted {
                    cause: expected_cause
                },
                Phase::Continuing
            ],
            "hard={hard}: restart must explain interruption then admit one continuation: {:?}",
            flow.items
        );
        let deltas = flow.deltas();
        let continuation = delta_position(&deltas, CONTINUATION_PREFIX);
        let first = delta_position(&deltas, "queued first");
        let second = delta_position(&deltas, "queued second");
        assert!(
            continuation < first && first < second,
            "hard={hard}: the continuation runs before the queue, which keeps its order: {deltas:?}"
        );
        assert_eq!(
            flow.queue_counts().last(),
            Some(&0),
            "hard={hard}: the queue must drain after the continuation: {:?}",
            flow.queue_counts()
        );

        let inputs = mock_inputs(&fixture, &held.agent_id).await;
        assert_eq!(
            inputs.len(),
            3,
            "hard={hard}: exactly one continuation joins the two queued messages: {inputs:?}"
        );
        assert_eq!(
            inputs[1..]
                .iter()
                .map(|input| (input.origin.clone(), input.message.as_str()))
                .collect::<Vec<_>>(),
            vec![(None, "queued first"), (None, "queued second")],
            "hard={hard}: queued messages reach the backend once each, in order"
        );
        assert_eq!(inputs[0].origin, Some(MessageOrigin::HostRestart));
        assert!(
            inputs[0].message.starts_with(CONTINUATION_PREFIX)
                && !inputs[0].message.contains("child agents"),
            "hard={hard}: an agent without children gets the plain continuation: {}",
            inputs[0].message
        );
        assert!(
            mock_inputs(&fixture, &idle.agent_id).await.is_empty(),
            "hard={hard}: an idle agent must not be continued"
        );
        assert!(
            stored_turn_recovery(&fixture, &session).is_null(),
            "hard={hard}: finishing the continued work spends the recovery marker"
        );
        let mut ids_after = fixture.agent_ids().await;
        ids_after.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            ids_after, ids_before,
            "hard={hard}: restart reopens exactly the same agents"
        );
    }
}

/// An orchestrator parked in `tyde_await_agents` on a child that is mid-turn
/// gets both back after a restart: the child keeps its id and continues
/// first, and only then is the parent continued, told that its children were
/// restored under the same ids, ahead of anything it had queued.
#[tokio::test]
async fn parent_continues_after_its_restored_children() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    let parent_replay = server::backend::mock::MockResumeReplay::default();
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "restart orchestrator",
        "orchestrate",
        None,
        MockScript::one(MockTurn::text("parent ready"))
            .with_controlled_resume_replay(&parent_replay),
    )
    .await;
    fixture.finish_turn(&parent).await;
    let child_replay = server::backend::mock::MockResumeReplay::default();
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "restart worker",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished"))
            .with_controlled_resume_replay(&child_replay),
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

    let bootstrap = fixture.restart_host().await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[parent_session.clone(), child_session.clone()],
        )
        .await;
    let (restored_parent, restored_child) = (&descriptors[0], &descriptors[1]);
    assert_eq!(restored_parent.agent_id, parent.new_agent.agent_id);
    assert_eq!(restored_child.agent_id, child.new_agent.agent_id);
    assert_eq!(
        restored_child.parent_agent_id.as_ref(),
        Some(&parent.new_agent.agent_id),
        "the restored child still belongs to the restored parent"
    );

    child_replay.wait_until_started().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(750),
            parent_replay.wait_until_started()
        )
        .await
        .is_err(),
        "the parent is not resumed while its child is still being restored"
    );
    child_replay.complete();
    parent_replay.wait_until_started().await;
    parent_replay.complete();
    let child_stream = restored_child.instance_stream.clone();
    let parent_stream = restored_parent.instance_stream.clone();
    let observation = Observation::observe_until(
        &mut fixture,
        &[child_stream.clone(), parent_stream.clone()],
        |observation| {
            observation
                .stream(&child_stream)
                .settled_after(CONTINUATION_PREFIX)
                && observation
                    .stream(&parent_stream)
                    .settled_after("parent queued")
        },
    )
    .await;
    let child_flow = observation.stream(&child_stream);
    let Some(Observed::Bootstrap {
        activity: child_activity,
        phases: child_bootstrap_phases,
    }) = child_flow.bootstrap()
    else {
        panic!(
            "the restored child must be bootstrapped: {:?}",
            child_flow.items
        );
    };
    assert_eq!(
        *child_activity,
        protocol::AgentActivity::Thinking,
        "the interrupted child reopens live"
    );
    assert_eq!(
        child_bootstrap_phases.first(),
        Some(&Phase::Interrupted {
            cause: Cause::HostRestart
        }),
        "{:?}",
        child_flow.items
    );
    assert_eq!(
        child_flow.phases(),
        vec![
            Phase::Interrupted {
                cause: Cause::HostRestart
            },
            Phase::Continuing
        ]
    );
    let parent_flow = observation.stream(&parent_stream);
    let Some(Observed::Bootstrap {
        activity: parent_activity,
        ..
    }) = parent_flow.bootstrap()
    else {
        panic!(
            "the restored parent must be bootstrapped: {:?}",
            parent_flow.items
        );
    };
    assert_eq!(
        *parent_activity,
        protocol::AgentActivity::Thinking,
        "a parent holding its continuation is live"
    );
    assert_eq!(
        parent_flow.phases(),
        vec![
            Phase::Interrupted {
                cause: Cause::HostRestart
            },
            Phase::Continuing
        ],
        "the parent is continued once its child has been: {:?}",
        parent_flow.items
    );
    assert_continued_before(&child_session, &parent_session);
    let parent_deltas = parent_flow.deltas();
    assert!(
        delta_position(&parent_deltas, CONTINUATION_PREFIX)
            < delta_position(&parent_deltas, "parent queued"),
        "the parent's continuation runs before its queued message: {parent_deltas:?}"
    );

    let child_inputs = mock_inputs(&fixture, &restored_child.agent_id).await;
    assert_eq!(child_inputs.len(), 1, "{child_inputs:?}");
    assert_eq!(child_inputs[0].origin, Some(MessageOrigin::HostRestart));
    assert!(!child_inputs[0].message.contains("child agents"));
    let parent_inputs = mock_inputs(&fixture, &restored_parent.agent_id).await;
    assert_eq!(parent_inputs.len(), 2, "{parent_inputs:?}");
    assert_eq!(parent_inputs[0].origin, Some(MessageOrigin::HostRestart));
    let notice = &parent_inputs[0].message;
    assert!(
        notice.starts_with(CONTINUATION_PREFIX)
            && notice.contains(&child.new_agent.agent_id.0)
            && notice.contains("tyde_await_agents")
            && notice.contains("do not spawn replacements"),
        "the orchestrator is told to reuse its restored children: {notice}"
    );
    assert_eq!(parent_inputs[1].message, "parent queued");
    let mut ids = fixture.agent_ids().await;
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        parent.new_agent.agent_id.clone(),
        child.new_agent.agent_id.clone(),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(ids, expected, "no replacement agent may appear");
}

/// With automatic restoration off, an interrupted session stays in History
/// with its recovery marker. Resuming it by hand reports the interruption but
/// never continues on its own, and spends the marker.
#[tokio::test]
async fn manual_resume_reports_interruption_without_continuing() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    let write_id = protocol::SettingsWriteId("resume-previous-off".to_owned());
    fixture
        .client
        .settings_write(protocol::SettingsWritePayload {
            write_id: write_id.clone(),
            ops: vec![protocol::SettingOp::Replace {
                path: "/resume_previous_agents".to_owned(),
                value: serde_json::json!(false),
                expected: protocol::SettingExpectation::Value {
                    value: serde_json::json!(true),
                },
            }],
        })
        .await
        .expect("disable automatic restoration");
    fixture::expect_settings_write_applied(&mut fixture.client, &write_id, "restoration off").await;
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "manual resume",
        "unfinished prompt",
        None,
        MockScript::one(MockTurn::held_text("unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&agent, "held turn streamed", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let bootstrap = fixture.restart_host().await;
    assert!(
        bootstrap.agents.is_empty(),
        "restoration off must not reopen agents: {:?}",
        bootstrap.agents
    );
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InterruptedByRestart"),
        "the interrupted session keeps its marker in History"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("manual resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume from history");
    let resumed: NewAgentPayload = expect_next_event(&mut fixture.client, "resumed NewAgent")
        .await
        .parse_payload()
        .expect("parse resumed NewAgent");
    let resumed_bootstrap: AgentBootstrapPayload = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed bootstrap",
    )
    .await
    .parse_payload()
    .expect("parse resumed bootstrap");
    assert_eq!(
        recovery_phases(&resumed_bootstrap.events),
        vec![Phase::Interrupted {
            cause: Cause::HostRestart
        }],
        "a manual resume reports the interruption and nothing more: {:?}",
        resumed_bootstrap.events
    );
    assert!(
        resumed_bootstrap.activity == protocol::AgentActivity::Idle,
        "a manual resume does not start a turn"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    loop {
        let Ok(Ok(Some(env))) =
            tokio::time::timeout_at(deadline, fixture.client.next_event()).await
        else {
            break;
        };
        if env.stream == resumed.instance_stream && env.kind == FrameKind::ChatEvent {
            let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
            assert!(
                !matches!(
                    event,
                    ChatEvent::RestartRecovery { .. } | ChatEvent::TypingStatusChanged(true)
                ),
                "a manual resume must never continue on its own: {event:?}"
            );
        }
    }
    let inputs = mock_inputs(&fixture, &resumed.agent_id).await;
    assert!(
        inputs
            .iter()
            .all(|input| input.origin != Some(MessageOrigin::HostRestart))
            && inputs.is_empty(),
        "no continuation may reach the backend on a manual resume: {inputs:?}"
    );
    assert!(
        stored_turn_recovery(&fixture, &session).is_null(),
        "reporting the interruption spends the marker"
    );
}

/// Wait for the restored descriptor of `session` on the current client.
async fn restored_descriptor(
    fixture: &mut Fixture,
    bootstrap: &settings_model::HostBootstrapPayload,
    session: &SessionId,
) -> NewAgentPayload {
    RestoredReplay::seed(bootstrap)
        .agents(fixture, std::slice::from_ref(session))
        .await
        .remove(0)
}

/// A turn the host has admitted is interrupted by a restart even when the
/// restart begins before the provider has accepted it: the turn is durably
/// in flight from the moment the host commits to it.
#[tokio::test]
async fn restart_during_turn_admission_continues_the_admitted_turn() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    let send_gate = server::backend::mock::MockGateHandle::new();
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "restart admission",
        "first prompt",
        None,
        MockScript::one(MockTurn::text("first done"))
            .then(MockTurn::held_text("admitted"))
            .with_send_gate(&send_gate),
    )
    .await;
    fixture.finish_turn(&agent).await;
    fixture
        .client
        .send_message(&agent.stream, "admitted prompt".to_owned())
        .await
        .expect("send the admitted prompt");
    send_gate.wait_until_entered().await;

    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let shutdown = tokio::spawn(async move { host.shutdown_for_restart().await });
    stop_gate.wait_until_entered().await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InterruptedByRestart"),
        "a turn handed to the provider is interrupted by the restart, accepted or not"
    );
    send_gate.release_one();
    stop_gate.release_one();
    tokio::time::timeout(Duration::from_secs(27), shutdown)
        .await
        .expect("restart stop is bounded")
        .expect("restart stop");

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    let observation =
        Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
            observation
                .stream(&stream)
                .settled_after(CONTINUATION_PREFIX)
        })
        .await;
    assert_eq!(
        observation.stream(&stream).phases(),
        vec![
            Phase::Interrupted {
                cause: Cause::HostRestart
            },
            Phase::Continuing
        ]
    );
    let inputs = mock_inputs(&fixture, &restored.agent_id).await;
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_eq!(inputs[0].origin, Some(MessageOrigin::HostRestart));
}

async fn record_claude_quota(fixture: &Fixture, used: u8) {
    use protocol::{
        BackendCapacityState, CapacityBucket, CapacityBucketId, CapacityCoverage, CapacityMeasure,
        CapacityReport, CapacityReset, CapacityScope, CapacitySource, CapacityWindow,
        ClaudeLimitType, ValueProvenance,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64;
    fixture
        .host_for_test()
        .record_backend_capacity_for_test(
            BackendKind::Claude,
            BackendCapacityState::Known {
                report: CapacityReport {
                    source: CapacitySource::ClaudeControlUsage,
                    observed_at_ms: None,
                    plan: None,
                    buckets: vec![CapacityBucket {
                        id: CapacityBucketId::Claude {
                            limit: ClaudeLimitType::FiveHour,
                        },
                        label: "Five hour".to_owned(),
                        measure: CapacityMeasure::UsedPercent {
                            used_percent: used,
                            remaining_percent: 100 - used,
                            provenance: ValueProvenance {
                                vendor_reported: true,
                            },
                        },
                        scope: CapacityScope::Account,
                        window: CapacityWindow::Rolling {
                            duration_minutes: 5 * 60,
                        },
                        reset: CapacityReset::At {
                            at_ms: now_ms + 5 * 60 * 60 * 1000,
                        },
                        status: None,
                    }],
                    coverage: CapacityCoverage::AllVendorBuckets,
                },
            },
        )
        .await;
}

/// A restored turn held by a usage pause is continued when the pause lifts,
/// before the messages the user had queued behind it.
#[tokio::test]
async fn usage_hold_releases_the_restart_continuation_before_the_queue() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .replace_setting("/usage_limits/enabled", true, false)
        .await
        .expect("enable usage limits");
    fixture
        .next_frame_matching("usage limits enabled", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await;
    let replay = server::backend::mock::MockResumeReplay::default();
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "usage held restart",
        "unfinished prompt",
        None,
        MockScript::one(MockTurn::held_text("unfinished")).with_controlled_resume_replay(&replay),
    )
    .await;
    for (count, message) in [(1, "queued first"), (2, "queued second")] {
        fixture
            .client
            .send_message(&agent.stream, message.to_owned())
            .await
            .expect("queue behind the held turn");
        fixture.expect_queued_messages(&agent, count).await;
    }

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    replay.wait_until_started().await;
    record_claude_quota(&fixture, 95).await;
    // The mailbox round trip makes the actor take one more turn of its loop,
    // which is where it adopts the quota reading and pauses.
    fixture.mock_by_id(&restored.agent_id).await;
    replay.complete();
    let held =
        Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
            observation.stream(&stream).bootstrap().is_some()
        })
        .await;
    assert_eq!(
        held.stream(&stream).phases(),
        vec![Phase::Interrupted {
            cause: Cause::HostRestart
        }],
        "a paused agent is not continued: {:?}",
        held.stream(&stream).items
    );
    fixture.mock_by_id(&restored.agent_id).await;
    assert!(
        mock_inputs(&fixture, &restored.agent_id).await.is_empty(),
        "the usage pause holds the continuation and the queue"
    );

    record_claude_quota(&fixture, 2).await;
    let observation =
        Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
            observation.stream(&stream).settled_after("queued second")
        })
        .await;
    let inputs = mock_inputs(&fixture, &restored.agent_id).await;
    assert_eq!(
        inputs
            .iter()
            .map(|input| (
                input.origin.clone(),
                input.message.starts_with(CONTINUATION_PREFIX)
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(MessageOrigin::HostRestart), true),
            (None, false),
            (None, false)
        ],
        "the continuation runs before the queue once the pause lifts: {inputs:?}"
    );
    assert_eq!(
        inputs[1..]
            .iter()
            .map(|input| input.message.as_str())
            .collect::<Vec<_>>(),
        vec!["queued first", "queued second"]
    );
    assert_eq!(
        observation.stream(&stream).phases(),
        vec![Phase::Continuing],
        "{:?}",
        observation.stream(&stream).items
    );
}

/// A message the user sends to a restored parent while its child is still
/// being restored waits behind the parent's continuation, and the parent is
/// reported as continuing only once the continuation reached its backend.
#[tokio::test]
async fn user_message_during_restoration_waits_for_the_continuation() {
    use protocol::{MessageOrigin, RestartRecoveryPhase as Phase};
    let mut fixture = Fixture::new().await;
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "restart orchestrator",
        "orchestrate",
        None,
        MockScript::one(MockTurn::text("parent ready")),
    )
    .await;
    fixture.finish_turn(&parent).await;
    let child_replay = server::backend::mock::MockResumeReplay::default();
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "restart worker",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished"))
            .with_controlled_resume_replay(&child_replay),
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

    let bootstrap = fixture.restart_host().await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[parent_session.clone(), child_session.clone()],
        )
        .await;
    let (restored_parent, restored_child) = (descriptors[0].clone(), descriptors[1].clone());
    child_replay.wait_until_started().await;
    fixture
        .client
        .send_message(&restored_parent.instance_stream, "user message".to_owned())
        .await
        .expect("message the restored parent");
    child_replay.complete();

    let parent_stream = restored_parent.instance_stream.clone();
    let child_stream = restored_child.instance_stream.clone();
    let mut inputs_at_continuing = None;
    let mut observation = Observation::default();
    while !observation
        .streams
        .get(&parent_stream)
        .is_some_and(|flow| flow.settled_after("user message"))
    {
        let step = Observation::observe_until(
            &mut fixture,
            &[parent_stream.clone(), child_stream.clone()],
            |step| !step.sequence.is_empty(),
        )
        .await;
        for (stream, item) in step.sequence {
            observation.record(&stream, item);
        }
        if inputs_at_continuing.is_none()
            && observation
                .streams
                .get(&parent_stream)
                .is_some_and(|flow| flow.phases().contains(&Phase::Continuing))
        {
            inputs_at_continuing = Some(mock_inputs(&fixture, &restored_parent.agent_id).await);
        }
    }
    let inputs_at_continuing =
        inputs_at_continuing.expect("the parent must report its continuation");
    assert_eq!(
        inputs_at_continuing
            .first()
            .map(|input| input.origin.clone()),
        Some(Some(MessageOrigin::HostRestart)),
        "Continuing is reported only once the continuation reached the backend: {inputs_at_continuing:?}"
    );
    let inputs = mock_inputs(&fixture, &restored_parent.agent_id).await;
    assert_eq!(
        inputs
            .iter()
            .map(|input| (
                input.origin.clone(),
                input.message.starts_with(CONTINUATION_PREFIX)
            ))
            .collect::<Vec<_>>(),
        vec![(Some(MessageOrigin::HostRestart), true), (None, false)],
        "the user's message runs after the continuation, which is not dropped: {inputs:?}"
    );
    assert_eq!(inputs[1].message, "user message");
    assert_eq!(
        observation
            .stream(&parent_stream)
            .phases()
            .iter()
            .filter(|phase| **phase == Phase::Continuing)
            .count(),
        1
    );
}

/// A parent whose backend resumes its interrupted turn by itself is not even
/// resumed until its restored child has been, and its self-started turn is
/// then reported as the continuation without a second one being sent.
#[tokio::test]
async fn self_starting_parent_waits_for_its_gated_child() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    let parent_replay = server::backend::mock::MockResumeReplay::default();
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "self starting parent",
        "orchestrate",
        None,
        MockScript::one(MockTurn::held_text("parent unfinished"))
            .with_controlled_resume_replay(&parent_replay),
    )
    .await;
    fixture
        .next_chat_event_matching(&parent, "parent held turn streamed", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let child_replay = server::backend::mock::MockResumeReplay::default();
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "gated worker",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished"))
            .with_controlled_resume_replay(&child_replay),
    )
    .await;
    fixture
        .next_chat_event_matching(&child, "child held turn streamed", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let bootstrap = fixture.restart_host().await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[parent_session.clone(), child_session.clone()],
        )
        .await;
    let (restored_parent, restored_child) = (descriptors[0].clone(), descriptors[1].clone());
    child_replay.wait_until_started().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(750),
            parent_replay.wait_until_started()
        )
        .await
        .is_err(),
        "a self-starting parent must not be resumed before its child is restored"
    );
    child_replay.complete();
    parent_replay.wait_until_started().await;
    parent_replay.start_live_turn("parent resumed itself");
    parent_replay.complete();
    parent_replay.finish_live_turn("parent resumed itself");
    let parent_stream = restored_parent.instance_stream.clone();
    let child_stream = restored_child.instance_stream.clone();
    let observation = Observation::observe_until(
        &mut fixture,
        &[parent_stream.clone(), child_stream.clone()],
        |observation| {
            observation
                .stream(&child_stream)
                .settled_after(CONTINUATION_PREFIX)
                && {
                    // The self-started turn began before the replay boundary,
                    // so its text arrives inside the bootstrap, which reports
                    // it as live; only its end streams afterwards.
                    let parent = observation.stream(&parent_stream);
                    parent
                        .items
                        .iter()
                        .position(|item| {
                            matches!(
                                item,
                                Observed::Bootstrap {
                                    activity: protocol::AgentActivity::Thinking,
                                    ..
                                }
                            )
                        })
                        .is_some_and(|bootstrap| {
                            parent.items[bootstrap..].contains(&Observed::Typing(false))
                        })
                }
        },
    )
    .await;
    assert_eq!(
        observation.stream(&parent_stream).phases(),
        vec![
            Phase::Interrupted {
                cause: Cause::HostRestart
            },
            Phase::Continuing
        ],
        "{:?}",
        observation.stream(&parent_stream).items
    );
    assert!(
        observation.phase_position(&child_stream, &Phase::Continuing)
            < observation.phase_position(&parent_stream, &Phase::Continuing),
        "the child continues before its parent: {:?}",
        observation.sequence
    );
    let parent_inputs = mock_inputs(&fixture, &restored_parent.agent_id).await;
    assert!(
        parent_inputs
            .iter()
            .all(|input| input.origin != Some(MessageOrigin::HostRestart)),
        "a self-started turn is the continuation; none may be sent on top: {parent_inputs:?}"
    );
}

/// One root whose restored child is still replaying does not hold back an
/// unrelated root: each subtree is released on its own.
#[tokio::test]
async fn independent_restored_roots_continue_without_each_other() {
    use protocol::RestartRecoveryPhase as Phase;
    let mut fixture = Fixture::new().await;
    let (blocked_root, blocked_root_session) = spawn_restart_agent(
        &mut fixture,
        "blocked root",
        "orchestrate",
        None,
        MockScript::one(MockTurn::held_text("blocked root unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&blocked_root, "blocked root held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let held_child_replay = server::backend::mock::MockResumeReplay::default();
    let (held_child, held_child_session) = spawn_restart_agent(
        &mut fixture,
        "held child",
        "work",
        Some(blocked_root.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("held child unfinished"))
            .with_controlled_resume_replay(&held_child_replay),
    )
    .await;
    fixture
        .next_chat_event_matching(&held_child, "held child held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (free_root, free_root_session) = spawn_restart_agent(
        &mut fixture,
        "free root",
        "free work",
        None,
        MockScript::one(MockTurn::held_text("free root unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&free_root, "free root held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let bootstrap = fixture.restart_host().await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[
                blocked_root_session.clone(),
                held_child_session.clone(),
                free_root_session.clone(),
            ],
        )
        .await;
    held_child_replay.wait_until_started().await;
    let free_stream = descriptors[2].instance_stream.clone();
    let observation = Observation::observe_until(
        &mut fixture,
        std::slice::from_ref(&free_stream),
        |observation| {
            observation
                .stream(&free_stream)
                .settled_after(CONTINUATION_PREFIX)
        },
    )
    .await;
    assert!(
        observation
            .stream(&free_stream)
            .phases()
            .contains(&Phase::Continuing),
        "{:?}",
        observation.stream(&free_stream).items
    );
    held_child_replay.complete();
}

/// A restored parent the pass skips because it was closed mid-pass leaves its
/// child to resolve ownership through the resume path, which refuses it. That
/// must not strand an unrelated root the pass already reconstructed.
#[tokio::test]
async fn skipped_restored_parent_does_not_strand_an_unrelated_root() {
    use protocol::RestartRecoveryPhase as Phase;
    let mut fixture = Fixture::new().await;
    let (free_root, free_root_session) = spawn_restart_agent(
        &mut fixture,
        "free root",
        "free work",
        None,
        MockScript::one(MockTurn::held_text("free root unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&free_root, "free root held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (closed_parent, closed_parent_session) = spawn_restart_agent(
        &mut fixture,
        "closed parent",
        "orchestrate",
        None,
        MockScript::one(MockTurn::text("parent done")),
    )
    .await;
    fixture.finish_turn(&closed_parent).await;
    let (orphaned_child, orphaned_child_session) = spawn_restart_agent(
        &mut fixture,
        "orphaned child",
        "work",
        Some(closed_parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("orphaned child unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&orphaned_child, "orphaned child held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let restoration_gate = server::new_spawn_operation_test_gate();
    let restoration_finished = server::new_spawn_operation_test_gate();
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config.restoration_snapshot_test_gate = Some(restoration_gate.shared());
            config.restoration_complete_test_gate = Some(restoration_finished.shared());
        })
        .await;
    restoration_gate.wait_until_entered().await;

    // Close the parent while the pass holds a snapshot that still marks it,
    // so the pass skips it with its child still pending.
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: closed_parent_session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume the parent while restoration is held");
    let resumed: NewAgentPayload =
        fixture::next_frame_matching_on(&mut fixture.client, "resumed parent NewAgent", |env| {
            env.kind == FrameKind::NewAgent
        })
        .await
        .parse_payload()
        .expect("parse resumed parent NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        "resumed parent start",
    )
    .await;
    fixture
        .client
        .close_agent(&resumed.instance_stream)
        .await
        .expect("close the resumed parent");
    fixture::next_frame_matching_on(&mut fixture.client, "resumed parent closed", |env| {
        env.kind == FrameKind::AgentClosed
            && env
                .parse_payload::<protocol::AgentClosedPayload>()
                .is_ok_and(|payload| payload.agent_id == resumed.agent_id)
    })
    .await;
    restoration_gate.release_one();

    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(&mut fixture, std::slice::from_ref(&free_root_session))
        .await;
    let free_stream = descriptors[0].instance_stream.clone();
    let observation = Observation::observe_until(
        &mut fixture,
        std::slice::from_ref(&free_stream),
        |observation| {
            observation
                .stream(&free_stream)
                .settled_after(CONTINUATION_PREFIX)
        },
    )
    .await;
    assert!(
        observation
            .stream(&free_stream)
            .phases()
            .contains(&Phase::Continuing),
        "{:?}",
        observation.stream(&free_stream).items
    );
    restoration_finished.wait_until_entered().await;
    let live_sessions = fixture.host_for_test().live_agent_session_ids().await;
    assert!(
        !live_sessions.contains(&closed_parent_session)
            && !live_sessions.contains(&orphaned_child_session),
        "a closed parent and the child it owns must not be restored"
    );
}

/// A restored child whose startup hangs past the bound fails through the
/// ordinary startup failure path, reporting its continuation failed, and
/// releases its parent. No parent waits on a root, so a root whose startup
/// outlasts the same bound still continues once it starts.
#[tokio::test]
async fn hung_restored_child_startup_releases_its_parent() {
    use protocol::RestartRecoveryPhase as Phase;
    let mut fixture = Fixture::new().await;
    let (free_root, free_root_session) = spawn_restart_agent(
        &mut fixture,
        "free root",
        "free work",
        None,
        MockScript::one(MockTurn::held_text("free root unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&free_root, "free root held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "waiting parent",
        "orchestrate",
        None,
        MockScript::one(MockTurn::held_text("parent unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&parent, "parent held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "hung child",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&child, "child held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let _hung_startup = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("hung child");
    let slow_root_startup = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("free root");
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config.restored_agent_startup_timeout = Some(Duration::from_secs(2));
        })
        .await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[
                free_root_session.clone(),
                parent_session.clone(),
                child_session.clone(),
            ],
        )
        .await;
    let streams = descriptors
        .iter()
        .map(|descriptor| descriptor.instance_stream.clone())
        .collect::<Vec<_>>();
    let (free_stream, parent_stream, child_stream) =
        (streams[0].clone(), streams[1].clone(), streams[2].clone());
    let child_failed = |observation: &Observation| {
        observation
            .stream(&child_stream)
            .phases()
            .iter()
            .any(|phase| matches!(phase, Phase::ContinuationFailed { message } if message.contains("timed out")))
    };
    let observation = Observation::observe_until(&mut fixture, &streams, |observation| {
        observation.stream(&parent_stream).continuation_finished() && child_failed(observation)
    })
    .await;
    assert!(
        observation
            .stream(&parent_stream)
            .phases()
            .contains(&Phase::Continuing),
        "{:?}",
        observation.stream(&parent_stream).items
    );
    assert!(
        !observation
            .stream(&child_stream)
            .phases()
            .contains(&Phase::Continuing),
        "a child whose startup timed out must not continue: {:?}",
        observation.stream(&child_stream).items
    );

    // The root activated alongside the child, so the bound has passed for it
    // too by the time the child failed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    slow_root_startup.release_one();
    let root_resolved = |observation: &Observation| {
        observation.stream(&free_stream).continuation_finished()
            || observation
                .stream(&free_stream)
                .phases()
                .iter()
                .any(|phase| matches!(phase, Phase::ContinuationFailed { .. }))
    };
    let observation =
        Observation::continue_until(observation, &mut fixture, &streams, root_resolved).await;
    let root_phases = observation.stream(&free_stream).phases();
    assert!(
        root_phases.contains(&Phase::Continuing)
            && !root_phases
                .iter()
                .any(|phase| matches!(phase, Phase::ContinuationFailed { .. })),
        "a slow root startup must continue late instead of failing: {:?}",
        observation.stream(&free_stream).items
    );
}

/// A backend whose startup workers outlive a dropped startup future cannot be
/// cut off by the restored-child bound: its parent stays held until the
/// child's startup actually ends, and the child then continues first.
#[tokio::test]
async fn non_cancellable_restored_child_startup_holds_its_parent() {
    use protocol::RestartRecoveryPhase as Phase;
    let mut fixture = Fixture::new().await;
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "held parent",
        "orchestrate",
        None,
        MockScript::one(MockTurn::held_text("parent unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&parent, "parent held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "uncancellable child",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&child, "child held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let host = fixture.host_for_test();
    host.install_non_cancellable_agent_startup("uncancellable child");
    let slow_startup = host.install_agent_startup_completion_test_gate("uncancellable child");
    let bound = Duration::from_secs(1);
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config.restored_agent_startup_timeout = Some(bound);
        })
        .await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[parent_session.clone(), child_session.clone()],
        )
        .await;
    let streams = descriptors
        .iter()
        .map(|descriptor| descriptor.instance_stream.clone())
        .collect::<Vec<_>>();
    let (parent_stream, child_stream) = (streams[0].clone(), streams[1].clone());
    slow_startup.wait_until_entered().await;
    tokio::time::sleep(bound * 3).await;
    slow_startup.release_one();
    let observation = Observation::observe_until(&mut fixture, &streams, |observation| {
        observation.stream(&parent_stream).continuation_finished()
            && (observation.stream(&child_stream).continuation_finished()
                || observation
                    .stream(&child_stream)
                    .phases()
                    .iter()
                    .any(|phase| matches!(phase, Phase::ContinuationFailed { .. })))
    })
    .await;
    let child_phases = observation.stream(&child_stream).phases();
    assert!(
        child_phases.contains(&Phase::Continuing)
            && !child_phases
                .iter()
                .any(|phase| matches!(phase, Phase::ContinuationFailed { .. })),
        "a startup that cannot be cancelled must not be timed out: {:?}",
        observation.stream(&child_stream).items
    );
    assert_continued_before(&child_session, &parent_session);
}

/// A restored parent whose spawn fails is not resumed under a new identity
/// behind its children. Its subtree keeps its restoration intent while an
/// unrelated root continues, and the next launch restores parent and child
/// under their original identities, child first.
#[tokio::test]
async fn failed_restored_parent_keeps_its_subtree_for_a_stable_retry() {
    use protocol::RestartRecoveryPhase as Phase;
    let mut fixture = Fixture::new().await;
    let (free_root, free_root_session) = spawn_restart_agent(
        &mut fixture,
        "free root",
        "free work",
        None,
        MockScript::one(MockTurn::held_text("free root unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&free_root, "free root held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (parent, parent_session) = spawn_restart_agent(
        &mut fixture,
        "failing parent",
        "orchestrate",
        None,
        MockScript::one(MockTurn::held_text("parent unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&parent, "parent held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let (child, child_session) = spawn_restart_agent(
        &mut fixture,
        "stranded child",
        "work",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::held_text("child unfinished")),
    )
    .await;
    fixture
        .next_chat_event_matching(&child, "child held", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;

    let restoration_started = server::new_spawn_operation_test_gate();
    let restoration_finished = server::new_spawn_operation_test_gate();
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config
                .rejected_restoration_sessions
                .insert(parent_session.clone());
            config.restoration_snapshot_test_gate = Some(restoration_started.shared());
            config.restoration_complete_test_gate = Some(restoration_finished.shared());
        })
        .await;
    restoration_started.wait_until_entered().await;
    // Connected while the pass is still running, so it can only learn the
    // outcome as a live status.
    let (mut early_client, early_bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(early_bootstrap.agent_restoration_failures.is_empty());
    restoration_started.release_one();
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(&mut fixture, std::slice::from_ref(&free_root_session))
        .await;
    let free_stream = descriptors[0].instance_stream.clone();
    let observation = Observation::observe_until(
        &mut fixture,
        std::slice::from_ref(&free_stream),
        |observation| observation.stream(&free_stream).continuation_finished(),
    )
    .await;
    assert!(
        observation
            .stream(&free_stream)
            .phases()
            .contains(&Phase::Continuing),
        "{:?}",
        observation.stream(&free_stream).items
    );
    restoration_finished.wait_until_entered().await;
    assert_eq!(
        fixture.agent_ids().await,
        vec![free_root.new_agent.agent_id.clone()],
        "a parent that failed to restore must not come back under a new identity"
    );
    let expected_failure = |failures: &[protocol::AgentRestorationFailure], context: &str| {
        assert_eq!(
            failures.len(),
            1,
            "{context}: one failed subtree is reported: {failures:?}"
        );
        let failure = &failures[0];
        assert!(
            matches!(
                &failure.agent,
                protocol::RestoredAgentRef::Session { session_id, agent_id, .. }
                    if session_id == &parent_session
                        && agent_id.as_ref() == Some(&parent.new_agent.agent_id)
            ),
            "{context}: the failed parent is identified: {failure:?}"
        );
        assert!(
            matches!(
                &failure.reason,
                protocol::AgentRestorationFailureReason::ReconstructFailed { message }
                    if message.contains("test rejected this restored spawn")
            ),
            "{context}: the reconstruction error is reported: {failure:?}"
        );
        assert_eq!(failure.retry, protocol::AgentRestorationRetry::NextLaunch);
        assert!(
            matches!(
                failure.skipped.as_slice(),
                [protocol::RestoredAgentRef::Session { session_id, agent_id, .. }]
                    if session_id == &child_session
                        && agent_id.as_ref() == Some(&child.new_agent.agent_id)
            ),
            "{context}: the stranded child is reported as skipped: {failure:?}"
        );
    };
    let (_new_client, new_bootstrap) = fixture.connect_with_bootstrap().await;
    expected_failure(
        &new_bootstrap.agent_restoration_failures,
        "a client connecting after the pass",
    );
    let status: protocol::AgentRestorationStatusPayload =
        fixture::next_frame_matching_on(&mut early_client, "live restoration status", |env| {
            env.kind == FrameKind::AgentRestorationStatus
        })
        .await
        .parse_payload()
        .expect("parse AgentRestorationStatus");
    expected_failure(&status.failures, "a client connected during the pass");
    restoration_finished.release_one();

    let bootstrap = fixture.restart_host().await;
    let mut restored = RestoredReplay::seed(&bootstrap);
    let descriptors = restored
        .agents(
            &mut fixture,
            &[parent_session.clone(), child_session.clone()],
        )
        .await;
    assert_eq!(descriptors[0].agent_id, parent.new_agent.agent_id);
    assert_eq!(descriptors[1].agent_id, child.new_agent.agent_id);
    let (parent_stream, child_stream) = (
        descriptors[0].instance_stream.clone(),
        descriptors[1].instance_stream.clone(),
    );
    let streams = [parent_stream.clone(), child_stream.clone()];
    Observation::observe_until(&mut fixture, &streams, |observation| {
        observation.stream(&parent_stream).continuation_finished()
            && observation.stream(&child_stream).continuation_finished()
    })
    .await;
    assert_continued_before(&child_session, &parent_session);
    assert!(bootstrap.agent_restoration_failures.is_empty());
    let (_client, after_retry) = fixture.connect_with_bootstrap().await;
    assert!(
        after_retry.agent_restoration_failures.is_empty(),
        "a successful retry reports no restoration failure: {:?}",
        after_retry.agent_restoration_failures
    );
}

/// Closing an agent in the middle of a turn withdraws it from recovery: a
/// later restart does not restore it and a resume from History reopens it
/// idle, reporting no interruption. That holds whether the turn is parked on a
/// question, the agent is still starting its first turn, or its backend
/// failed mid turn and left it parked terminal.
#[tokio::test]
async fn closing_mid_turn_spends_the_recovery_marker() {
    let mut fixture = Fixture::new().await;
    let gate = server::backend::mock::MockGateHandle::new();
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "closed mid turn",
        "first prompt",
        None,
        MockScript::one(MockTurn::text("launch response")).then(
            MockTurn::blocking_question_request("closed-question", &gate),
        ),
    )
    .await;
    fixture.finish_turn(&agent).await;
    fixture
        .client
        .send_message(&agent.stream, "ask me".to_owned())
        .await
        .expect("send the turn that asks");
    gate.wait_until_entered().await;
    gate.release_one();
    fixture
        .expect_paused_tool_request(&agent, "AskUserQuestion")
        .await;
    fixture.mock(&agent).await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InFlight"),
        "a turn parked on a question is in flight"
    );

    close_and_expect_no_recovery(
        &mut fixture,
        &agent.stream,
        &agent.new_agent.agent_id,
        &session,
        "closed mid turn",
        None,
    )
    .await;

    // A restored agent closed while it is still starting, before it could
    // continue the turn the restart interrupted.
    let mut fixture = Fixture::new().await;
    let name = "closed during startup";
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        name,
        "first prompt",
        None,
        MockScript::one(MockTurn::held_text("unfinished work")),
    )
    .await;
    fixture
        .next_chat_event_matching(&agent, "held turn streamed", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let ready_gate = fixture
        .host_for_test()
        .install_agent_startup_backend_ready_test_gate(name);
    let bootstrap = fixture.restart_host().await;
    let restored = match bootstrap
        .agents
        .iter()
        .find(|agent| agent.session_id.as_ref() == Some(&session))
    {
        Some(agent) => agent.clone(),
        None => fixture
            .next_frame_matching("restored NewAgent", |env| {
                env.kind == FrameKind::NewAgent
                    && env
                        .parse_payload::<NewAgentPayload>()
                        .is_ok_and(|agent| agent.session_id.as_ref() == Some(&session))
            })
            .await
            .parse_payload()
            .expect("parse restored NewAgent"),
    };
    ready_gate.wait_until_entered().await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InterruptedByRestart"),
        "the restored agent is still starting, its interrupted turn not yet continued"
    );
    close_and_expect_no_recovery(
        &mut fixture,
        &restored.instance_stream,
        &restored.agent_id,
        &session,
        name,
        Some(ready_gate),
    )
    .await;

    let mut fixture = Fixture::new().await;
    let gate = server::backend::mock::MockGateHandle::new();
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "closed after failure",
        "first prompt",
        None,
        MockScript::one(MockTurn::text("launch response"))
            .then(MockTurn::busy_then_close_stream(&gate)),
    )
    .await;
    fixture.finish_turn(&agent).await;
    fixture
        .client
        .send_message(&agent.stream, "fail mid turn".to_owned())
        .await
        .expect("send the turn that fails");
    gate.wait_until_entered().await;
    gate.release_one();
    fixture
        .next_frame_matching("fatal backend failure", |env| {
            env.stream == agent.stream
                && env.kind == FrameKind::AgentError
                && env
                    .parse_payload::<protocol::AgentErrorPayload>()
                    .is_ok_and(|payload| payload.fatal)
        })
        .await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InFlight"),
        "a turn its backend failed is left in flight until the agent is closed"
    );
    close_and_expect_no_recovery(
        &mut fixture,
        &agent.stream,
        &agent.new_agent.agent_id,
        &session,
        "closed after failure",
        None,
    )
    .await;

    // Closed after the host began stopping for a restart: the restart has
    // already marked the turn interrupted, and the close must still spend it.
    let mut fixture = Fixture::new().await;
    let gate = server::backend::mock::MockGateHandle::new();
    let name = "closed while restarting";
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        name,
        "first prompt",
        None,
        MockScript::one(MockTurn::text("launch response")).then(
            MockTurn::blocking_question_request("restarting-question", &gate),
        ),
    )
    .await;
    fixture.finish_turn(&agent).await;
    fixture
        .client
        .send_message(&agent.stream, "ask me".to_owned())
        .await
        .expect("send the turn that asks");
    gate.wait_until_entered().await;
    gate.release_one();
    fixture
        .expect_paused_tool_request(&agent, "AskUserQuestion")
        .await;
    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let stopping_host = fixture.host_for_test();
    let stop = tokio::spawn(async move {
        stopping_host.shutdown_for_restart().await;
    });
    stop_gate.wait_until_entered().await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::json!("InterruptedByRestart"),
        "the stopping host marks the parked turn interrupted before any agent stops"
    );
    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close the agent while the host is stopping");
    fixture
        .next_frame_matching("AgentClosed while restarting", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|payload| payload.agent_id == agent.new_agent.agent_id)
        })
        .await;
    assert!(
        stored_turn_recovery(&fixture, &session).is_null(),
        "{name}: an explicit close withdraws the recovery marker the stopping host wrote"
    );
    stop_gate.release_one();
    stop.await.expect("restart stop task");
    expect_closed_agent_not_continued(&mut fixture, &session, name).await;
}

async fn close_and_expect_no_recovery(
    fixture: &mut Fixture,
    stream: &protocol::StreamPath,
    agent_id: &protocol::AgentId,
    session: &SessionId,
    name: &str,
    startup_gate: Option<server::InstalledSpawnOperationTestGate>,
) {
    fixture
        .client
        .close_agent(stream)
        .await
        .expect("close the agent");
    fixture
        .next_frame_matching("AgentClosed", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|payload| &payload.agent_id == agent_id)
        })
        .await;
    // The closed agent's startup never took its release; let later starts
    // under the same name pass.
    drop(startup_gate);
    expect_closed_agent_not_continued(fixture, session, name).await;
}

async fn expect_closed_agent_not_continued(fixture: &mut Fixture, session: &SessionId, name: &str) {
    assert!(
        stored_turn_recovery(fixture, session).is_null(),
        "{name}: a closed agent is never continued"
    );
    let bootstrap = fixture.restart_host().await;
    assert!(
        !bootstrap
            .agents
            .iter()
            .any(|agent| agent.session_id.as_ref() == Some(session)),
        "{name}: a closed agent is not restored by a restart"
    );
    assert!(stored_turn_recovery(fixture, session).is_null());

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume from history");
    let resumed: NewAgentPayload = fixture
        .next_frame_matching("resumed NewAgent", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|agent| agent.session_id.as_ref() == Some(session))
        })
        .await
        .parse_payload()
        .expect("parse resumed NewAgent");
    let resumed_bootstrap: AgentBootstrapPayload = expect_raw_event_on_stream(
        &mut fixture.client,
        &resumed.instance_stream,
        FrameKind::AgentBootstrap,
        "resumed bootstrap",
    )
    .await
    .parse_payload()
    .expect("parse resumed bootstrap");
    assert!(
        recovery_phases(&resumed_bootstrap.events).is_empty(),
        "{name}: a closed turn is not reported as interrupted: {:?}",
        resumed_bootstrap.events
    );
    assert_eq!(resumed_bootstrap.activity, protocol::AgentActivity::Idle);
    assert!(stored_turn_recovery(fixture, session).is_null());
}

/// A tool running when the host stops, cleanly or by a hard kill, is closed
/// with an unknown outcome after the restart, exactly once, instead of being
/// shown as running forever. That holds wherever the provider reports the
/// request relative to its response, including inside a response the host
/// died before seeing end, and the restored transcript shows the call once.
#[tokio::test]
async fn tools_running_at_restart_end_with_an_unknown_outcome() {
    for shape in [
        RunningCommandShape::RequestAfterResponse,
        RunningCommandShape::RequestInsideEndedResponse,
        RunningCommandShape::RequestInsideOpenResponse,
    ] {
        for hard in [false, true] {
            let mut fixture = Fixture::new().await;
            let tool_call_id = "restart-running-command";
            let (agent, session) = spawn_restart_agent(
                &mut fixture,
                "restart running tool",
                "run it",
                None,
                MockScript::one(MockTurn::text("ready"))
                    .then(MockTurn::held_running_command(tool_call_id, shape)),
            )
            .await;
            // The first idle makes the transcript authoritative, so the restored
            // history carries the unfinished tool call the way a provider's own
            // session log carries a tool use that never got its result.
            fixture.finish_turn(&agent).await;
            fixture
                .client
                .send_message(&agent.stream, "start the command".to_owned())
                .await
                .expect("start the running command");
            fixture
                .next_chat_event_matching(&agent, "command started", |event| {
                    matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == tool_call_id)
                })
                .await;
            if matches!(shape, RunningCommandShape::RequestInsideEndedResponse) {
                fixture
                    .next_chat_event_matching(
                        &agent,
                        "response issuing the command ended",
                        |event| matches!(event, ChatEvent::StreamEnd(_)),
                    )
                    .await;
            }
            fixture.mock(&agent).await;

            let bootstrap = if hard {
                fixture.relaunch_host_after_kill().await
            } else {
                fixture.restart_host().await
            };
            let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
            let stream = restored.instance_stream.clone();
            let mut outcomes = Vec::new();
            let mut requests = 0;
            let mut flow = StreamObservation::default();
            while !flow.settled_after(CONTINUATION_PREFIX) {
                let env = fixture::next_frame_matching_on(
                    &mut fixture.client,
                    "restored running-tool activity",
                    |env| env.stream == stream,
                )
                .await;
                match env.kind {
                    FrameKind::AgentBootstrap => {
                        let payload: AgentBootstrapPayload =
                            env.parse_payload().expect("parse restored bootstrap");
                        outcomes.extend(restart_expiry_of(&payload.events, tool_call_id));
                        requests += payload
                            .events
                            .iter()
                            .filter(|event| {
                                matches!(
                                    event,
                                    AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(request))
                                        if request.tool_call_id == tool_call_id
                                )
                            })
                            .count();
                    }
                    FrameKind::ChatEvent => {
                        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                        match &event {
                            ChatEvent::ToolExecutionCompleted(completion)
                                if completion.tool_call_id == tool_call_id =>
                            {
                                outcomes.push(completion.outcome.clone());
                            }
                            ChatEvent::ToolRequest(request)
                                if request.tool_call_id == tool_call_id =>
                            {
                                requests += 1;
                            }
                            _ => {}
                        }
                        flow.record_chat_event(event);
                    }
                    _ => {}
                }
            }
            assert_eq!(
                requests, 1,
                "{shape:?} hard={hard}: the restored transcript shows the running command once"
            );
            assert_eq!(
                outcomes.len(),
                1,
                "{shape:?} hard={hard}: the running tool ends exactly once: {outcomes:?}"
            );
            assert!(
                matches!(
                    &outcomes[0],
                    protocol::ToolExecutionOutcome::Cancelled { message }
                        if message.contains("outcome is unknown")
                ),
                "{shape:?} hard={hard}: the tool's outcome is unknown, not failed or succeeded: {:?}",
                outcomes[0]
            );
        }
    }
}

/// The tool calls in `events`, in order, as the client would draw them.
fn tool_trace<'a>(events: impl IntoIterator<Item = &'a ChatEvent>) -> Vec<String> {
    events
        .into_iter()
        .filter_map(|event| match event {
            ChatEvent::ToolRequest(request) => Some(format!("request {}", request.tool_call_id)),
            ChatEvent::ToolExecutionCompleted(completion) => {
                let outcome = match &completion.outcome {
                    protocol::ToolExecutionOutcome::Cancelled { message }
                        if message.contains("outcome is unknown") =>
                    {
                        "unknown".to_owned()
                    }
                    protocol::ToolExecutionOutcome::Succeeded { .. } => "succeeded".to_owned(),
                    protocol::ToolExecutionOutcome::Failed { .. } => "failed".to_owned(),
                    other => format!("{other:?}"),
                };
                Some(format!("{outcome} {}", completion.tool_call_id))
            }
            _ => None,
        })
        .collect()
}

fn bootstrap_chat_events(payload: &AgentBootstrapPayload) -> Vec<ChatEvent> {
    payload
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(event) => Some(event.clone()),
            _ => None,
        })
        .collect()
}

/// A response the backend abandons without ending it, by interrupting a tool
/// or by opening its next response, keeps the commands it issued in history
/// the way live clients saw them: a reconnecting client sees each call once,
/// before its completion. After a clean restart or a hard kill the idle
/// session ends the command that never finished with an unknown outcome, keeps
/// the reported outcome of the other exactly once, and does not continue on
/// its own.
#[tokio::test]
async fn abandoned_response_commands_stay_in_history_and_end_after_restart() {
    for (abandon, hard) in [
        (AbandonedResponse::Interrupted, false),
        (AbandonedResponse::Interrupted, true),
        (AbandonedResponse::Replaced, false),
        (AbandonedResponse::Replaced, true),
    ] {
        let mut fixture = Fixture::new().await;
        let running_id = "abandoned-running-command";
        let finished_id = "abandoned-finished-command";
        let (agent, session) =
            spawn_restart_agent(
                &mut fixture,
                "abandoned response",
                "get ready",
                None,
                MockScript::one(MockTurn::text("ready")).then(
                    MockTurn::abandoned_response_commands(running_id, finished_id, abandon),
                ),
            )
            .await;
        fixture.finish_turn(&agent).await;
        fixture
            .client
            .send_message(&agent.stream, "abandon a response".to_owned())
            .await
            .expect("send the abandoned turn");
        let live = fixture.finish_turn(&agent).await;
        let live_events = live
            .frames
            .iter()
            .filter(|env| env.kind == FrameKind::ChatEvent)
            .map(|env| env.parse_payload::<ChatEvent>().expect("parse ChatEvent"))
            .collect::<Vec<_>>();
        let finished_outcome = match abandon {
            AbandonedResponse::Interrupted => "failed",
            AbandonedResponse::Replaced => "succeeded",
        };
        let history = vec![
            format!("request {running_id}"),
            format!("request {finished_id}"),
            format!("{finished_outcome} {finished_id}"),
        ];
        assert_eq!(
            tool_trace(&live_events),
            history,
            "{abandon:?} hard={hard}: the live client saw both commands and one finish"
        );

        let mut other = fixture.connect().await;
        let env = fixture::next_frame_matching_on(
            &mut other,
            "reconnected bootstrap of the abandoned response",
            |env| env.kind == FrameKind::AgentBootstrap,
        )
        .await;
        let reconnected: AgentBootstrapPayload =
            env.parse_payload().expect("parse reconnected bootstrap");
        assert_eq!(
            tool_trace(&bootstrap_chat_events(&reconnected)),
            history,
            "{abandon:?} hard={hard}: a reconnecting client sees the same commands the live client saw"
        );
        drop(other);

        let bootstrap = if hard {
            fixture.relaunch_host_after_kill().await
        } else {
            fixture.restart_host().await
        };
        let mut replay = RestoredReplay::seed(&bootstrap);
        let restored = replay
            .agents(&mut fixture, std::slice::from_ref(&session))
            .await
            .remove(0);
        let stream = restored.instance_stream.clone();
        let restored_bootstrap = replay.bootstrap(&mut fixture, &stream).await;
        let mut events = bootstrap_chat_events(&restored_bootstrap);
        fixture
            .client
            .send_message(&stream, "after restart".to_owned())
            .await
            .expect("send after the restart");
        let mut flow = StreamObservation::default();
        while !flow.settled_after("after restart") {
            let env = fixture::next_frame_matching_on(
                &mut fixture.client,
                "restored abandoned-response activity",
                |env| env.stream == stream && env.kind == FrameKind::ChatEvent,
            )
            .await;
            let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
            events.push(event.clone());
            flow.record_chat_event(event);
        }
        let mut restored_history = history.clone();
        restored_history.push(format!("unknown {running_id}"));
        assert_eq!(
            tool_trace(&events),
            restored_history,
            "{abandon:?} hard={hard}: after the restart each command shows once, the reported \
             outcome is kept once, and the unfinished command ends with an unknown outcome"
        );
        assert!(
            recovery_phases(&restored_bootstrap.events).is_empty() && flow.phases().is_empty(),
            "{abandon:?} hard={hard}: an idle session reports no restart recovery: {:?} {:?}",
            recovery_phases(&restored_bootstrap.events),
            flow.phases()
        );
        let inputs = mock_inputs(&fixture, &restored.agent_id).await;
        assert_eq!(
            inputs
                .iter()
                .map(|input| input.message.as_str())
                .collect::<Vec<_>>(),
            vec!["after restart"],
            "{abandon:?} hard={hard}: the restored idle session sends the provider only the user's message"
        );
        assert!(
            stored_turn_recovery(&fixture, &session).is_null(),
            "{abandon:?} hard={hard}: the idle session never carried a recovery marker"
        );
    }
}

fn interrupted_session_under(fixture: &Fixture, workspace_root: &str) -> Option<SessionId> {
    let store = SessionStore::load(fixture.session_store_path()).expect("load session store");
    store
        .list()
        .expect("list stored sessions")
        .into_iter()
        .find(|record| {
            record
                .workspace_roots
                .iter()
                .any(|root| root == workspace_root)
                && serde_json::to_value(record.turn_recovery).expect("inspect turn recovery")
                    == serde_json::json!("InterruptedByRestart")
        })
        .map(|record| record.id)
}

/// A restart that lands while a new agent's first prompt is already with the
/// provider records that turn as interrupted and continues it on the restored
/// agent, followed by the message the user sent while it was starting.
#[tokio::test]
async fn restart_during_initial_prompt_startup_continues_the_spawned_turn() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    let mut fixture = Fixture::new().await;
    let name = "startup restart";
    let workspace_root = "/tmp/startup-restart";
    let reservation = fixture
        .reserve_next_mock_launch(name, MockScript::one(MockTurn::held_text("startup work")))
        .await;
    let ready_gate = fixture
        .host_for_test()
        .install_agent_startup_backend_ready_test_gate(name);
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root.to_owned()],
                prompt: "first prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");
    let spawned: NewAgentPayload = fixture
        .next_frame_matching("NewAgent", |env| env.kind == FrameKind::NewAgent)
        .await
        .parse_payload()
        .expect("parse NewAgent");
    ready_gate.wait_until_entered().await;
    drop(reservation);
    fixture
        .client
        .send_message(&spawned.instance_stream, "sent during startup".to_owned())
        .await
        .expect("send while starting");
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions");
    fixture
        .next_frame_matching("SessionList after the startup message", |env| {
            env.kind == FrameKind::SessionList
        })
        .await;

    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let shutdown = tokio::spawn(async move { host.shutdown_for_restart().await });
    stop_gate.wait_until_entered().await;
    stop_gate.release_one();
    ready_gate.release_one();
    tokio::time::timeout(Duration::from_secs(27), shutdown)
        .await
        .expect("restart stop is bounded")
        .expect("restart stop");
    let session = interrupted_session_under(&fixture, workspace_root)
        .expect("the first turn the provider started is recorded as interrupted by the restart");

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    let observation =
        Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
            observation
                .stream(&stream)
                .settled_after("sent during startup")
        })
        .await;
    assert_eq!(
        observation.stream(&stream).phases(),
        vec![
            Phase::Interrupted {
                cause: Cause::HostRestart
            },
            Phase::Continuing
        ]
    );
    let inputs = mock_inputs(&fixture, &restored.agent_id).await;
    assert_eq!(
        inputs
            .iter()
            .map(|input| (
                input.origin == Some(MessageOrigin::HostRestart),
                input.message.starts_with(CONTINUATION_PREFIX)
            ))
            .collect::<Vec<_>>(),
        vec![(true, true), (false, false)],
        "the continuation runs first, then the startup message: {inputs:?}"
    );
    assert_eq!(inputs[1].message, "sent during startup");
}

/// Messages accepted while a restored agent is still replaying survive a
/// second restart before the replay finishes, and run in order after the
/// continuation.
#[tokio::test]
async fn messages_held_by_replay_survive_a_second_restart() {
    use protocol::MessageOrigin;
    let mut fixture = Fixture::new().await;
    let replay = server::backend::mock::MockResumeReplay::default();
    let (_agent, session) = spawn_restart_agent(
        &mut fixture,
        "replay held restart",
        "unfinished prompt",
        None,
        MockScript::one(MockTurn::held_text("unfinished")).with_controlled_resume_replay(&replay),
    )
    .await;

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    replay.wait_until_started().await;
    for message in ["held first", "held second"] {
        fixture
            .client
            .send_message(&restored.instance_stream, message.to_owned())
            .await
            .expect("send while replaying");
    }
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions");
    fixture
        .next_frame_matching("SessionList after the held messages", |env| {
            env.kind == FrameKind::SessionList
        })
        .await;
    fixture.mock_by_id(&restored.agent_id).await;

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
        observation.stream(&stream).settled_after("held second")
    })
    .await;
    let inputs = mock_inputs(&fixture, &restored.agent_id).await;
    assert_eq!(
        inputs
            .iter()
            .map(|input| (
                input.origin == Some(MessageOrigin::HostRestart),
                input.message.starts_with(CONTINUATION_PREFIX)
            ))
            .collect::<Vec<_>>(),
        vec![(true, true), (false, false), (false, false)],
        "the held messages survive the second restart behind the continuation: {inputs:?}"
    );
    assert_eq!(
        inputs[1..]
            .iter()
            .map(|input| input.message.as_str())
            .collect::<Vec<_>>(),
        vec!["held first", "held second"]
    );
}

/// Delivers `message` to a child through the parent's agent-control
/// `tyde_send_agent_message`, the acknowledged delivery path.
async fn deliver_agent_message(
    fixture: &Fixture,
    parent: &protocol::AgentId,
    child: &protocol::AgentId,
    message: &str,
) {
    let response = agent_control_delivery(fixture, parent, child, message).await;
    assert_eq!(
        response["result"]["isError"],
        serde_json::Value::Bool(false),
        "agent-control delivery failed: {response}"
    );
}

/// The agent-control response to delivering `message` to a child through the
/// parent's `tyde_send_agent_message`.
async fn agent_control_delivery(
    fixture: &Fixture,
    parent: &protocol::AgentId,
    child: &protocol::AgentId,
    message: &str,
) -> serde_json::Value {
    let caller = fixture.agent_control_caller(parent).await;
    let body = reqwest::Client::new()
        .post(&caller.url)
        .header("Authorization", &caller.authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_send_agent_message",
                "arguments": { "agent_id": child.0, "message": message }
            }
        }))
        .send()
        .await
        .expect("post agent-control request")
        .text()
        .await
        .expect("read agent-control response");
    serde_json::from_str(
        body.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("agent-control SSE data line"),
    )
    .expect("parse agent-control response")
}

/// A message queued on an idle agent held by its usage quota is not a turn in
/// flight: a restart redelivers it without continuing anything.
#[tokio::test]
async fn quota_queued_message_on_idle_agent_is_not_continued_after_restart() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .replace_setting("/usage_limits/enabled", true, false)
        .await
        .expect("enable usage limits");
    fixture
        .next_frame_matching("usage limits enabled", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await;
    let (parent, _parent_session) = spawn_restart_agent(
        &mut fixture,
        "quota idle parent",
        "parent prompt",
        None,
        MockScript::one(MockTurn::text("parent done")),
    )
    .await;
    fixture.finish_turn(&parent).await;
    let (agent, session) = spawn_restart_agent(
        &mut fixture,
        "quota idle restart",
        "first prompt",
        Some(parent.new_agent.agent_id.clone()),
        MockScript::one(MockTurn::text("first done")),
    )
    .await;
    fixture.finish_turn(&agent).await;
    let agent_id = agent.new_agent.agent_id.clone();
    record_claude_quota(&fixture, 95).await;
    fixture.mock_by_id(&agent_id).await;
    deliver_agent_message(
        &fixture,
        &parent.new_agent.agent_id,
        &agent_id,
        "held by quota",
    )
    .await;
    fixture.expect_queued_messages(&agent, 1).await;
    fixture.mock_by_id(&agent_id).await;

    fixture.host_for_test().shutdown_for_restart().await;
    assert_eq!(
        stored_turn_recovery(&fixture, &session),
        serde_json::Value::Null,
        "a queued message on an idle agent is not a turn the restart interrupted"
    );
    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    let observation =
        Observation::observe_until(&mut fixture, std::slice::from_ref(&stream), |observation| {
            observation.stream(&stream).settled_after("held by quota")
        })
        .await;
    assert_eq!(
        observation.stream(&stream).phases(),
        Vec::new(),
        "{:?}",
        observation.stream(&stream).items
    );
    let inputs = mock_inputs(&fixture, &restored.agent_id).await;
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.message.as_str())
            .collect::<Vec<_>>(),
        vec!["held by quota"]
    );
}

/// A continuation the provider refuses because it started a turn of its own
/// stays owed: it is reported only once accepted, runs before the queue, and
/// survives a restart that lands after that turn ended but before it is
/// re-sent.
#[tokio::test]
async fn busy_provider_retains_the_restart_continuation() {
    use protocol::{
        MessageOrigin, RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase,
    };
    for restart_before_resend in [false, true] {
        let mut fixture = Fixture::new().await;
        let replay = server::backend::mock::MockResumeReplay::default();
        let send_gate = server::backend::mock::MockGateHandle::new();
        replay.gate_sends(&send_gate);
        let (agent, session) = spawn_restart_agent(
            &mut fixture,
            "busy continuation",
            "unfinished prompt",
            None,
            MockScript::one(MockTurn::held_text("unfinished"))
                .with_controlled_resume_replay(&replay),
        )
        .await;
        fixture
            .client
            .send_message(&agent.stream, "queued behind".to_owned())
            .await
            .expect("queue behind the held turn");
        fixture.expect_queued_messages(&agent, 1).await;

        let bootstrap = fixture.restart_host().await;
        let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
        let stream = restored.instance_stream.clone();
        replay.wait_until_started().await;
        let mock = fixture.mock_by_id(&restored.agent_id).await;
        replay.complete();
        send_gate.wait_until_entered().await;
        let resend_gate = fixture
            .host_for_test()
            .install_restart_continuation_dispatch_test_gate(&restored.name);
        send_gate.release_one_busy();
        let busy = Observation::observe_until(
            &mut fixture,
            std::slice::from_ref(&stream),
            |observation| {
                observation
                    .stream(&stream)
                    .settled_after("self-initiated wakeup")
            },
        )
        .await;
        assert_eq!(
            busy.stream(&stream).phases(),
            vec![Phase::Interrupted {
                cause: Cause::HostRestart
            }],
            "restart_before_resend={restart_before_resend}: a refused continuation is not \
             continuing: {:?}",
            busy.stream(&stream).items
        );
        resend_gate.wait_until_entered().await;

        let final_agent = if restart_before_resend {
            let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
            let host = fixture.host_for_test();
            let shutdown = tokio::spawn(async move { host.shutdown_for_restart().await });
            stop_gate.wait_until_entered().await;
            assert_eq!(
                stored_turn_recovery(&fixture, &session),
                serde_json::json!("InterruptedByRestart"),
                "the undelivered continuation is still owed when the restart lands"
            );
            resend_gate.release_one();
            stop_gate.release_one();
            tokio::time::timeout(Duration::from_secs(27), shutdown)
                .await
                .expect("restart stop is bounded")
                .expect("restart stop");
            let bootstrap = fixture.restart_host().await;
            restored_descriptor(&mut fixture, &bootstrap, &session).await
        } else {
            resend_gate.release_one();
            send_gate.wait_until_entered().await;
            assert!(
                !mock
                    .requests()
                    .await
                    .into_iter()
                    .any(|request| matches!(request, server::backend::mock::MockRequest::Input(_))),
                "the queue stays behind the undelivered continuation"
            );
            send_gate.release_one();
            send_gate.wait_until_entered().await;
            send_gate.release_one();
            restored
        };
        let stream = final_agent.instance_stream.clone();
        let observation = Observation::observe_until(
            &mut fixture,
            std::slice::from_ref(&stream),
            |observation| observation.stream(&stream).settled_after("queued behind"),
        )
        .await;
        let expected_phases = if restart_before_resend {
            vec![
                Phase::Interrupted {
                    cause: Cause::HostRestart,
                },
                Phase::Interrupted {
                    cause: Cause::HostRestart,
                },
                Phase::Continuing,
            ]
        } else {
            vec![Phase::Continuing]
        };
        assert_eq!(
            observation.stream(&stream).phases(),
            expected_phases,
            "restart_before_resend={restart_before_resend}: {:?}",
            observation.stream(&stream).items
        );
        let inputs = mock_inputs(&fixture, &final_agent.agent_id).await;
        assert_eq!(
            inputs
                .iter()
                .map(|input| (
                    input.origin == Some(MessageOrigin::HostRestart),
                    input.message.as_str() == "queued behind"
                ))
                .collect::<Vec<_>>(),
            vec![(true, false), (false, true)],
            "restart_before_resend={restart_before_resend}: the continuation runs before the \
             queue: {inputs:?}"
        );
    }
}

/// A turn that cannot be durably recorded as in flight is refused, not started
/// unrecoverably.
#[tokio::test]
async fn unrecordable_turn_is_refused() {
    let mut fixture = Fixture::new().await;
    let (agent, _session) = spawn_restart_agent(
        &mut fixture,
        "unrecordable turn",
        "first prompt",
        None,
        MockScript::one(MockTurn::text("first done")).then(MockTurn::text("never")),
    )
    .await;
    fixture.finish_turn(&agent).await;
    let agent_id = agent.new_agent.agent_id.clone();
    fixture.mock_by_id(&agent_id).await;
    let failure = fixture.fail_next_session_commit();
    fixture
        .client
        .send_message(&agent.stream, "unrecordable".to_owned())
        .await
        .expect("send the unrecordable message");
    let error: AgentErrorPayload = fixture
        .next_frame_matching("AgentError for the unrecorded turn", |env| {
            env.stream == agent.stream && env.kind == FrameKind::AgentError
        })
        .await
        .parse_payload()
        .expect("parse AgentError");
    drop(failure);
    assert!(
        error.message.contains("cannot record the turn") && !error.fatal,
        "{error:?}"
    );
    fixture.mock_by_id(&agent_id).await;
    assert!(
        mock_inputs(&fixture, &agent_id)
            .await
            .iter()
            .all(|input| input.message != "unrecordable"),
        "an unrecorded turn never reaches the provider"
    );
}

/// The messages a live agent's provider has received, its launch prompt
/// first, once there are at least `count` of them.
async fn wait_for_provider_messages(
    fixture: &Fixture,
    agent_id: &protocol::AgentId,
    count: usize,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut messages = Vec::new();
    while tokio::time::Instant::now() < deadline {
        if let Some(mock) = fixture.host_for_test().mock_control(agent_id).await {
            messages = mock
                .requests()
                .await
                .into_iter()
                .filter_map(|request| match request {
                    server::backend::mock::MockRequest::Launch { message } => Some(message),
                    server::backend::mock::MockRequest::Input(input) => Some(input.message),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if messages.len() >= count {
                return messages;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the provider received {} of {count} messages: {messages:?}",
        messages.len()
    );
}

/// Spawns `params` under `name` with its backend startup held at the ready
/// gate, returning the gate and the spawned descriptor once startup is held.
async fn spawn_held_at_startup(
    fixture: &mut Fixture,
    name: &str,
    parent_agent_id: Option<protocol::AgentId>,
    params: SpawnAgentParams,
) -> (server::InstalledSpawnOperationTestGate, NewAgentPayload) {
    let ready_gate = fixture
        .host_for_test()
        .install_agent_startup_backend_ready_test_gate(name);
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id,
            project_id: None,
            params,
        })
        .await
        .expect("spawn agent");
    let spawned: NewAgentPayload = fixture
        .next_frame_matching("NewAgent for the held startup", |env| {
            env.kind == FrameKind::NewAgent
        })
        .await
        .parse_payload()
        .expect("parse NewAgent");
    ready_gate.wait_until_entered().await;
    (ready_gate, spawned)
}

fn new_agent_params(workspace_root: &str, prompt: &str) -> SpawnAgentParams {
    SpawnAgentParams::New {
        workspace_roots: vec![workspace_root.to_owned()],
        prompt: prompt.to_owned(),
        images: None,
        backend_kind: BackendKind::Claude,
        launch_profile_id: None,
        cost_hint: None,
        access_mode: Default::default(),
        session_settings: None,
    }
}

/// A host killed while a new agent is still starting, before its provider
/// session exists, restores that agent: its first prompt runs, followed by the
/// message acknowledged while it was starting.
#[tokio::test]
async fn hard_kill_during_new_agent_startup_restores_its_first_turn() {
    let mut fixture = Fixture::new().await;
    let (parent, _parent_session) = spawn_restart_agent(
        &mut fixture,
        "startup kill parent",
        "parent prompt",
        None,
        MockScript::one(MockTurn::text("parent done")),
    )
    .await;
    fixture.finish_turn(&parent).await;
    let parent_id = parent.new_agent.agent_id.clone();
    let (ready_gate, child) = spawn_held_at_startup(
        &mut fixture,
        "startup kill child",
        Some(parent_id.clone()),
        new_agent_params("/tmp/startup-kill-child", "first prompt"),
    )
    .await;
    deliver_agent_message(&fixture, &parent_id, &child.agent_id, "sent during startup").await;

    fixture.relaunch_host_after_kill().await;
    drop(ready_gate);
    assert_eq!(
        wait_for_provider_messages(&fixture, &child.agent_id, 2).await,
        vec!["first prompt", "sent during startup"],
        "the restored agent runs its first prompt, then the acknowledged message"
    );
}

/// A host killed while a fork is still starting restores the fork and runs
/// its first prompt.
#[tokio::test]
async fn hard_kill_during_fork_startup_restores_its_first_turn() {
    let mut fixture = Fixture::new().await;
    let (source, source_session) = spawn_restart_agent(
        &mut fixture,
        "fork kill source",
        "source prompt",
        None,
        MockScript::one(MockTurn::text("source done")),
    )
    .await;
    fixture.finish_turn(&source).await;
    let (ready_gate, fork) = spawn_held_at_startup(
        &mut fixture,
        "fork kill child",
        None,
        SpawnAgentParams::Fork {
            from_session_id: source_session,
            prompt: "fork prompt".to_owned(),
            images: None,
            access_mode: None,
        },
    )
    .await;

    fixture.relaunch_host_after_kill().await;
    drop(ready_gate);
    assert_eq!(
        wait_for_provider_messages(&fixture, &fork.agent_id, 1).await,
        vec!["fork prompt"],
        "the restored fork runs its first prompt"
    );
}

/// A message delivered to a starting agent is acknowledged only once it is
/// durably held; one the store refuses is rejected and never sent.
#[tokio::test]
async fn unretained_startup_delivery_is_rejected() {
    let mut fixture = Fixture::new().await;
    let (parent, _parent_session) = spawn_restart_agent(
        &mut fixture,
        "unretained startup parent",
        "parent prompt",
        None,
        MockScript::one(MockTurn::text("parent done")),
    )
    .await;
    fixture.finish_turn(&parent).await;
    let parent_id = parent.new_agent.agent_id.clone();
    let (ready_gate, child) = spawn_held_at_startup(
        &mut fixture,
        "unretained startup child",
        Some(parent_id.clone()),
        new_agent_params("/tmp/unretained-startup-child", "first prompt"),
    )
    .await;
    let failure = fixture.fail_next_session_commit();
    let response =
        agent_control_delivery(&fixture, &parent_id, &child.agent_id, "unretained").await;
    drop(failure);
    assert_eq!(
        response["result"]["isError"],
        serde_json::Value::Bool(true),
        "an unretained startup delivery must not be acknowledged: {response}"
    );
    assert!(
        response.to_string().contains("cannot record the turn"),
        "{response}"
    );
    deliver_agent_message(&fixture, &parent_id, &child.agent_id, "retained").await;
    drop(ready_gate);
    assert_eq!(
        wait_for_provider_messages(&fixture, &child.agent_id, 2).await,
        vec!["first prompt", "retained"],
        "the rejected message never reaches the provider"
    );
}

/// A message sent while a restored agent is replaying is held only once it is
/// durable; one the store refuses is rejected and never sent.
#[tokio::test]
async fn unretained_replay_delivery_is_rejected() {
    let mut fixture = Fixture::new().await;
    let replay = server::backend::mock::MockResumeReplay::default();
    let (_agent, session) = spawn_restart_agent(
        &mut fixture,
        "unretained replay",
        "unfinished prompt",
        None,
        MockScript::one(MockTurn::held_text("unfinished")).with_controlled_resume_replay(&replay),
    )
    .await;

    let bootstrap = fixture.restart_host().await;
    let restored = restored_descriptor(&mut fixture, &bootstrap, &session).await;
    let stream = restored.instance_stream.clone();
    replay.wait_until_started().await;
    fixture
        .client
        .send_message(&stream, "held first".to_owned())
        .await
        .expect("send while replaying");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let store = SessionStore::load(fixture.session_store_path()).expect("load session store");
        let held = store.get(&session).is_some_and(|record| {
            record
                .queued_messages
                .iter()
                .any(|entry| entry.message == "held first")
        });
        if held {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first replay message is held durably"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (consumed_tx, consumed) = tokio::sync::oneshot::channel();
    let failure = server::store::session::commit_hooks::InstalledHook::install(
        SessionStore::database_path(&fixture.session_store_path()),
        Box::new(move || {
            let _ = consumed_tx.send(());
            Err("injected session commit failure".to_owned())
        }),
    );
    fixture
        .client
        .send_message(&stream, "unretained".to_owned())
        .await
        .expect("send while replaying");
    tokio::time::timeout(Duration::from_secs(10), consumed)
        .await
        .expect("the replay message's commit is attempted")
        .expect("commit hook signal");
    fixture.mock_by_id(&restored.agent_id).await;
    replay.complete();
    // Attachment waits for the replay, so the rejection may arrive inside the
    // agent's bootstrap or live after it.
    let env = fixture
        .next_frame_matching("the unretained message's rejection", |env| {
            env.stream == stream
                && (env.kind == FrameKind::AgentError
                    || env.kind == FrameKind::AgentBootstrap
                        && env
                            .parse_payload::<AgentBootstrapPayload>()
                            .is_ok_and(|payload| {
                                payload.events.iter().any(|event| {
                                    matches!(event, AgentBootstrapEvent::AgentError(_))
                                })
                            }))
        })
        .await;
    let error = match env.kind {
        FrameKind::AgentError => env
            .parse_payload::<AgentErrorPayload>()
            .expect("parse AgentError"),
        _ => env
            .parse_payload::<AgentBootstrapPayload>()
            .expect("parse AgentBootstrap")
            .events
            .into_iter()
            .find_map(|event| match event {
                AgentBootstrapEvent::AgentError(error) => Some(error),
                _ => None,
            })
            .expect("bootstrap AgentError"),
    };
    assert!(
        error.message.contains("cannot record the turn") && !error.fatal,
        "{error:?}"
    );
    drop(failure);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let inputs = loop {
        let inputs = mock_inputs(&fixture, &restored.agent_id).await;
        if inputs.iter().any(|input| input.message == "held first")
            || tokio::time::Instant::now() >= deadline
        {
            break inputs;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        inputs.last().map(|input| input.message.as_str()),
        Some("held first"),
        "{inputs:?}"
    );
    assert!(
        inputs.iter().all(|input| input.message != "unretained"),
        "the rejected message never reaches the provider: {inputs:?}"
    );
}

/// An explicit close of a starting agent wins over the restart that is
/// waiting for its startup: the next launch does not reconstruct it.
#[tokio::test]
async fn close_during_startup_with_deferred_restart_is_not_restored() {
    let mut fixture = Fixture::new().await;
    let (ready_gate, spawned) = spawn_held_at_startup(
        &mut fixture,
        "startup close restart",
        None,
        new_agent_params("/tmp/startup-close-restart", "first prompt"),
    )
    .await;
    let stop_gate = fixture.host_for_test().install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let shutdown = tokio::spawn(async move { host.shutdown_for_restart().await });
    stop_gate.wait_until_entered().await;
    fixture
        .client
        .close_agent(&spawned.instance_stream)
        .await
        .expect("close the starting agent");
    fixture
        .next_frame_matching("AgentClosed for the starting agent", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|payload| payload.agent_id == spawned.agent_id)
        })
        .await;
    stop_gate.release_one();
    tokio::time::timeout(Duration::from_secs(27), shutdown)
        .await
        .expect("restart stop is bounded")
        .expect("restart stop");

    let restoration_finished = server::new_spawn_operation_test_gate();
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config.restoration_complete_test_gate = Some(restoration_finished.shared());
        })
        .await;
    restoration_finished.wait_until_entered().await;
    assert!(
        bootstrap
            .agents
            .iter()
            .all(|agent| agent.agent_id != spawned.agent_id)
            && !fixture.agent_ids().await.contains(&spawned.agent_id),
        "a closed agent must not be restored"
    );
    drop(ready_gate);
}
