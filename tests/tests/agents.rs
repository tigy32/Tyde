mod fixture;

use fixture::Fixture;
use protocol::{
    AgentStartPayload, BackendKind, ChatEvent, Envelope, FrameKind, NewAgentPayload, Project,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectId,
    ProjectNotifyPayload, ProjectRenamePayload, SpawnAgentParams, SpawnAgentPayload,
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
async fn agent_lifecycle() {
    let mut fixture = Fixture::new().await;

    // 1. Spawn an agent
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "test-agent".to_owned(),
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
        .expect("spawn_agent failed");

    // 2. Receive NewAgent on host stream
    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before NewAgent");

    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload");
    assert!(!new_agent.agent_id.0.is_empty());
    assert_eq!(new_agent.backend_kind, BackendKind::Claude);
    assert_eq!(new_agent.name, "test-agent");
    let agent_stream = new_agent.instance_stream.clone();

    // 3. Receive AgentStart
    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before AgentStart");

    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);
    assert_eq!(env.seq, 0);

    let start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload");
    assert!(!start.agent_id.0.is_empty());
    assert_eq!(start.backend_kind, BackendKind::Claude);
    assert_eq!(start.name, "test-agent");

    // 4. Receive mock's initial turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    // 5. Send a follow-up message
    fixture
        .client
        .send_message(&agent_stream, "follow up".to_owned())
        .await
        .expect("send_message failed");

    // 6. Receive follow-up turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(&mut fixture.client, "mock backend response to: follow up").await;
}

#[tokio::test]
async fn multiple_agents() {
    let mut fixture = Fixture::new().await;

    // Spawn two agents
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "first".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: Some("agent one".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn first agent failed");

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "second".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: Some("agent two".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn second agent failed");

    // Collect all events from both agents.
    // Each agent produces:
    //   NewAgent (host stream) + AgentStart + StreamStart + StreamDelta + StreamEnd
    // Two agents = 10 events total.
    let mut events = Vec::new();
    for _ in 0..10 {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("next_event failed")
            .expect("connection closed before all events received");
        events.push(env);
    }

    let new_agent_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == FrameKind::NewAgent)
        .collect();
    assert_eq!(new_agent_events.len(), 2, "expected 2 NewAgent events");

    // Collect unique agent streams from NewAgent payloads
    let streams: std::collections::HashSet<String> = new_agent_events
        .iter()
        .map(|env| {
            let payload: NewAgentPayload = env
                .parse_payload()
                .expect("failed to parse NewAgentPayload");
            payload.instance_stream.0
        })
        .collect();
    assert_eq!(
        streams.len(),
        2,
        "expected events on exactly 2 agent streams"
    );

    // For each stream, verify the agent event sequence
    for stream in &streams {
        let stream_events: Vec<_> = events
            .iter()
            .filter(|e| e.stream.0 == *stream && e.kind != FrameKind::NewAgent)
            .collect();

        assert_eq!(
            stream_events.len(),
            4,
            "expected 4 events on stream {stream}",
        );

        // First event must be AgentStart at seq 0
        assert_eq!(stream_events[0].kind, FrameKind::AgentStart);
        assert_eq!(stream_events[0].seq, 0);

        // Remaining 3 must be ChatEvents with sequential seqs
        for (i, env) in stream_events[1..].iter().enumerate() {
            assert_eq!(env.kind, FrameKind::ChatEvent);
            assert_eq!(env.seq, (i + 1) as u64);
        }

        // Parse the ChatEvents: StreamStart, StreamDelta, StreamEnd
        let event: ChatEvent = stream_events[1]
            .parse_payload()
            .expect("failed to parse StreamStart");
        assert!(matches!(event, ChatEvent::StreamStart(..)));

        let event: ChatEvent = stream_events[2]
            .parse_payload()
            .expect("failed to parse StreamDelta");
        assert!(matches!(event, ChatEvent::StreamDelta(..)));

        let event: ChatEvent = stream_events[3]
            .parse_payload()
            .expect("failed to parse StreamEnd");
        assert!(matches!(event, ChatEvent::StreamEnd(..)));
    }
}

#[tokio::test]
async fn late_joining_client_gets_replay() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "replay-agent".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: Some("late join replay".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    // Client 1: NewAgent on host stream.
    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before NewAgent");
    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let client1_new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload for client 1");
    let agent_id = client1_new_agent.agent_id.clone();
    let client1_instance_stream = client1_new_agent.instance_stream.clone();

    // Client 1: AgentStart replay baseline.
    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before AgentStart");
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, client1_instance_stream);
    assert_eq!(env.seq, 0);

    let client1_start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload for client 1");
    assert_eq!(client1_start.agent_id, agent_id);

    // Client 1: StreamStart -> StreamDelta -> StreamEnd.
    let mut client1_chat_payloads = Vec::new();

    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before StreamStart");
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamStart for client 1");
    assert!(matches!(event, ChatEvent::StreamStart(..)));
    client1_chat_payloads.push(env.payload.clone());

    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before StreamDelta");
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamDelta for client 1");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta
                    .text
                    .contains("mock backend response to: late join replay"),
                "unexpected StreamDelta text for client 1: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta for client 1, got {other:?}"),
    }
    client1_chat_payloads.push(env.payload.clone());

    let env = fixture
        .client
        .next_event()
        .await
        .expect("next_event failed")
        .expect("connection closed before StreamEnd");
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamEnd for client 1");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
    client1_chat_payloads.push(env.payload.clone());

    // Client 2 connects late and should receive NewAgent + full replay on its own instance stream.
    let mut client2 = fixture.connect().await;

    let env = expect_next_event(&mut client2, "NewAgent for client 2").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let client2_new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload for client 2");
    assert_eq!(client2_new_agent.agent_id, agent_id);
    assert_ne!(
        client2_new_agent.instance_stream, client1_instance_stream,
        "late-joining client must get a distinct instance stream",
    );
    let client2_instance_stream = client2_new_agent.instance_stream.clone();

    let env = expect_next_event(&mut client2, "AgentStart for client 2").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, client2_instance_stream);
    assert_eq!(env.seq, 0, "replayed AgentStart must be seq 0");

    let client2_start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload for client 2");
    assert_eq!(client2_start.agent_id, agent_id);
    assert_eq!(client2_start.name, client1_start.name);
    assert_eq!(client2_start.backend_kind, client1_start.backend_kind);
    assert_eq!(client2_start.workspace_roots, client1_start.workspace_roots);
    assert_eq!(client2_start.parent_agent_id, client1_start.parent_agent_id);
    assert_eq!(client2_start.created_at_ms, client1_start.created_at_ms);

    let mut client2_chat_payloads = Vec::new();

    let env = expect_next_event(&mut client2, "StreamStart for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamStart for client 2");
    assert!(matches!(event, ChatEvent::StreamStart(..)));
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "StreamDelta for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamDelta for client 2");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta
                    .text
                    .contains("mock backend response to: late join replay"),
                "unexpected StreamDelta text for client 2: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta for client 2, got {other:?}"),
    }
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "StreamEnd for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamEnd for client 2");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
    client2_chat_payloads.push(env.payload.clone());

    assert_eq!(
        client2_chat_payloads.len(),
        client1_chat_payloads.len(),
        "late-joining client should replay same number of ChatEvents",
    );
    assert_eq!(
        client2_chat_payloads, client1_chat_payloads,
        "replayed ChatEvent payloads must match original client payloads",
    );
}

#[tokio::test]
async fn project_mutations_fan_out_and_delete() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Tyde".to_owned(),
            roots: vec!["/tmp/tyde".to_owned()],
        })
        .await
        .expect("project_create failed");

    let created = match expect_project_notify(&mut fixture.client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    };
    assert_eq!(created.name, "Tyde");
    assert_eq!(created.roots, vec!["/tmp/tyde".to_owned()]);

    let mut client2 = fixture.connect().await;
    let replayed = match expect_project_notify(&mut client2, "project replay on connect").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected replayed upsert project notification, got {other:?}"),
    };
    assert_eq!(replayed, created);

    fixture
        .client
        .project_rename(ProjectRenamePayload {
            id: created.id.clone(),
            name: "Tyde Renamed".to_owned(),
        })
        .await
        .expect("project_rename failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project rename").await {
            ProjectNotifyPayload::Upsert { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(project.name, "Tyde Renamed");
                assert_eq!(project.roots, vec!["/tmp/tyde".to_owned()]);
            }
            other => panic!("expected renamed project notification, got {other:?}"),
        }
    }

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: created.id.clone(),
            root: "/tmp/tyde-extra".to_owned(),
        })
        .await
        .expect("project_add_root failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project add root").await {
            ProjectNotifyPayload::Upsert { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(
                    project.roots,
                    vec!["/tmp/tyde".to_owned(), "/tmp/tyde-extra".to_owned()]
                );
            }
            other => panic!("expected root-added project notification, got {other:?}"),
        }
    }

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: created.id.clone(),
        })
        .await
        .expect("project_delete failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project delete").await {
            ProjectNotifyPayload::Delete { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(project.name, "Tyde Renamed");
                assert_eq!(
                    project.roots,
                    vec!["/tmp/tyde".to_owned(), "/tmp/tyde-extra".to_owned()]
                );
            }
            other => panic!("expected deleted project notification, got {other:?}"),
        }
    }

    let mut client3 = fixture.connect().await;
    expect_no_event(
        &mut client3,
        Duration::from_millis(150),
        "deleted project should not replay to new clients",
    )
    .await;
}

#[tokio::test]
async fn project_replay_happens_before_agent_replay() {
    let mut fixture = Fixture::new().await;

    let project = create_project(
        &mut fixture.client,
        "Project Agent",
        vec!["/tmp/project-agent".to_owned()],
    )
    .await;
    let sibling = create_project(
        &mut fixture.client,
        "Project Sibling",
        vec!["/tmp/project-sibling".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "project-agent".to_owned(),
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project.roots.clone(),
                prompt: Some("hello from project".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn agent with project failed");

    let env = expect_next_event(&mut fixture.client, "project new agent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse project NewAgent");
    assert_eq!(new_agent.project_id.as_ref(), Some(&project.id));

    let env = expect_next_event(&mut fixture.client, "project agent start").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let start: AgentStartPayload = env.parse_payload().expect("parse project AgentStart");
    assert_eq!(start.project_id.as_ref(), Some(&project.id));

    expect_turn(
        &mut fixture.client,
        "mock backend response to: hello from project",
    )
    .await;

    let mut client2 = fixture.connect().await;

    let replayed_first = expect_project_notify(&mut client2, "first project replay").await;
    let replayed_second = expect_project_notify(&mut client2, "second project replay").await;
    let replayed_projects = vec![replayed_first, replayed_second]
        .into_iter()
        .map(|payload| match payload {
            ProjectNotifyPayload::Upsert { project } => project,
            other => panic!("expected replayed upsert project notification, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(replayed_projects, vec![project.clone(), sibling]);

    let env = expect_next_event(&mut client2, "new agent after project replay").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let replayed_agent: NewAgentPayload = env.parse_payload().expect("parse replayed NewAgent");
    assert_eq!(replayed_agent.project_id.as_ref(), Some(&project.id));

    let env = expect_next_event(&mut client2, "agent start after project replay").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let replayed_start: AgentStartPayload = env.parse_payload().expect("parse replayed AgentStart");
    assert_eq!(replayed_start.project_id.as_ref(), Some(&project.id));
}

#[tokio::test]
async fn projects_persist_to_disk_and_replay_from_fresh_host() {
    let mut fixture = Fixture::new().await;

    let project_a = create_project(
        &mut fixture.client,
        "Persist A",
        vec!["/tmp/persist-a".to_owned()],
    )
    .await;
    let project_b = create_project(
        &mut fixture.client,
        "Persist B",
        vec![
            "/tmp/persist-b".to_owned(),
            "/tmp/persist-b-extra".to_owned(),
        ],
    )
    .await;

    let mut fresh_client = fixture.connect_fresh_host().await;

    let replayed_a = match expect_project_notify(&mut fresh_client, "persisted project A").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected persisted upsert project notification, got {other:?}"),
    };
    let replayed_b = match expect_project_notify(&mut fresh_client, "persisted project B").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected persisted upsert project notification, got {other:?}"),
    };

    assert_eq!(vec![replayed_a, replayed_b], vec![project_a, project_b]);
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "fresh host should replay exactly the persisted projects",
    )
    .await;
}

#[tokio::test]
async fn project_delete_is_rejected_when_a_session_still_references_it() {
    let mut fixture = Fixture::new().await;

    let project = create_project(
        &mut fixture.client,
        "Delete Guard",
        vec!["/tmp/delete-guard".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "delete-guard-agent".to_owned(),
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project.roots.clone(),
                prompt: Some("hold project".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn delete guard agent failed");

    let _ = expect_next_event(&mut fixture.client, "delete guard NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "delete guard AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: hold project",
    )
    .await;

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete write failed");

    let closed = fixture
        .client
        .next_event()
        .await
        .expect("next_event after rejected delete failed");
    assert!(
        closed.is_none(),
        "deleting a referenced project should terminate the connection",
    );

    let mut fresh_client = fixture.connect_fresh_host().await;
    match expect_project_notify(&mut fresh_client, "project survives rejected delete").await {
        ProjectNotifyPayload::Upsert { project: replayed } => assert_eq!(replayed, project),
        other => panic!("expected surviving project upsert after rejected delete, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_project_input_closes_the_connection() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec!["/tmp/dup".to_owned(), "/tmp/dup".to_owned()],
        })
        .await
        .expect("project_create write failed");

    let closed = fixture
        .client
        .next_event()
        .await
        .expect("next_event after invalid project_create failed");
    assert!(
        closed.is_none(),
        "invalid project_create should terminate the connection",
    );

    let mut fresh_client = fixture.connect_fresh_host().await;
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "invalid project_create should not persist any project",
    )
    .await;
}

#[tokio::test]
async fn spawn_with_missing_project_id_closes_the_connection() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: "missing-project-agent".to_owned(),
            parent_agent_id: None,
            project_id: Some(ProjectId("11111111-1111-1111-1111-111111111111".to_owned())),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/missing-project".to_owned()],
                prompt: Some("hello".to_owned()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
            },
        })
        .await
        .expect("spawn_agent write failed");

    let closed = fixture
        .client
        .next_event()
        .await
        .expect("next_event after missing project spawn failed");
    assert!(
        closed.is_none(),
        "spawning with a missing project should terminate the connection",
    );

    let mut fresh_client = fixture.connect_fresh_host().await;
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "missing-project spawn should not persist any project state",
    )
    .await;
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
