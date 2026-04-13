mod fixture;

use fixture::Fixture;
use protocol::{
    AgentStartPayload, BackendKind, ChatEvent, Envelope, FrameKind, ListSessionsPayload,
    NewAgentPayload, Project, ProjectCreatePayload, ProjectNotifyPayload, SessionListPayload,
    SpawnAgentParams, SpawnAgentPayload,
};
use std::time::Duration;

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn expect_turn(client: &mut client::Connection, expected_text: &str) {
    let env = expect_next_event(client, "StreamStart").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamStart(..)));

    let env = expect_next_event(client, "StreamDelta").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let env = expect_next_event(client, "StreamEnd").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    match tokio::time::timeout(duration, client.next_event()).await {
        Err(_) => {}
        Ok(Ok(None)) => {}
        Ok(Ok(Some(env))) => panic!(
            "unexpected event before {context}: kind={} stream={}",
            env.kind, env.stream
        ),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
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
            name: "resumable".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: Some("hello".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn resumable agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _: NewAgentPayload = env.parse_payload().expect("parse NewAgent");

    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");

    let env = expect_next_event(&mut fixture.client, "SessionList").await;
    assert_eq!(env.kind, FrameKind::SessionList);
    let list: SessionListPayload = env.parse_payload().expect("parse SessionList");
    assert_eq!(list.sessions.len(), 1, "expected one stored session");
    let session = &list.sessions[0];
    assert_eq!(session.backend_kind, BackendKind::Claude);
    assert_eq!(session.workspace_roots, vec!["/tmp/test".to_owned()]);
    assert!(session.resumable);
    assert_eq!(session.message_count, 1);

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "resumed".to_owned(),
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

    let env = expect_next_event(&mut fixture.client, "resumed AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, resumed.instance_stream);

    expect_turn(
        &mut fixture.client,
        "mock backend response to: after resume",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after resume failed");

    let env = expect_next_event(&mut fixture.client, "SessionList after resume").await;
    let list: SessionListPayload = env.parse_payload().expect("parse SessionList after resume");
    assert_eq!(
        list.sessions.len(),
        1,
        "resume should reuse the same session"
    );
    assert_eq!(list.sessions[0].id, session.id);
    assert_eq!(list.sessions[0].message_count, 2);
}

#[tokio::test]
async fn session_listing_covers_empty_parent_child_and_resume_without_prompt() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("initial list_sessions failed");

    let env = expect_next_event(&mut fixture.client, "initial empty SessionList").await;
    assert_eq!(env.kind, FrameKind::SessionList);
    let list: SessionListPayload = env.parse_payload().expect("parse initial SessionList");
    assert!(
        list.sessions.is_empty(),
        "expected no sessions before any spawn"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "parent".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/parent".to_owned()],
                prompt: Some("parent hello".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    let parent_new_agent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let _ = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: parent hello",
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "child".to_owned(),
            parent_agent_id: Some(parent_new_agent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/child".to_owned()],
                prompt: Some("child hello".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn child failed");

    let _ = expect_next_event(&mut fixture.client, "child NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "child AgentStart").await;
    expect_turn(&mut fixture.client, "mock backend response to: child hello").await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions with parent/child failed");

    let env = expect_next_event(&mut fixture.client, "SessionList with parent/child").await;
    assert_eq!(env.kind, FrameKind::SessionList);
    let list: SessionListPayload = env.parse_payload().expect("parse parent/child SessionList");
    assert_eq!(
        list.sessions.len(),
        2,
        "expected two sessions in a single SessionList event"
    );

    let parent = list
        .sessions
        .iter()
        .find(|session| session.alias.as_deref() == Some("parent"))
        .expect("missing parent session in SessionList");
    let child = list
        .sessions
        .iter()
        .find(|session| session.alias.as_deref() == Some("child"))
        .expect("missing child session in SessionList");
    assert_eq!(
        child.parent_id.as_ref(),
        Some(&parent.id),
        "child session should point to parent session id",
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "resumed-parent".to_owned(),
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
    let env = expect_next_event(&mut fixture.client, "resumed parent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, resumed_parent.instance_stream);

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
            name: "project-session".to_owned(),
            parent_agent_id: None,
            project_id: Some(project_a.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_a.roots.clone(),
                prompt: Some("session project".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
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

    let env = expect_next_event(&mut fixture.client, "SessionList after project spawn").await;
    let list: SessionListPayload = env.parse_payload().expect("parse project SessionList");
    assert_eq!(list.sessions.len(), 1);
    let session = &list.sessions[0];
    assert_eq!(session.project_id.as_ref(), Some(&project_a.id));

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "resume-same-project".to_owned(),
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
    let env = expect_next_event(&mut fixture.client, "resume same project AgentStart").await;
    let resumed_same_start: AgentStartPayload =
        env.parse_payload().expect("parse resumed same AgentStart");
    assert_eq!(resumed_same_start.project_id.as_ref(), Some(&project_a.id));
    expect_turn(&mut fixture.client, "mock backend response to: resume same").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "resume-other-project".to_owned(),
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
    let env = expect_next_event(&mut fixture.client, "resume other project AgentStart").await;
    let resumed_other_start: AgentStartPayload =
        env.parse_payload().expect("parse resumed other AgentStart");
    assert_eq!(resumed_other_start.project_id.as_ref(), Some(&project_b.id));
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

    let env = expect_next_event(&mut fixture.client, "SessionList after override").await;
    let list: SessionListPayload = env
        .parse_payload()
        .expect("parse SessionList after override");
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
            roots,
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create helper").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}
