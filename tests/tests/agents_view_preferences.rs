mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentAnnotationTarget, AgentBootstrapEvent, AgentBootstrapPayload, AgentClosedPayload,
    AgentGroupAssignment, AgentGroupId, AgentGroupMode, AgentGroupsUpdate, AgentId,
    AgentListDensity, AgentManualTagId, AgentOrderKey, AgentPinsUpdate, AgentSortMode,
    AgentStartPayload, AgentStatusFilter, AgentSystemTagId, AgentTagColor, AgentTagRef,
    AgentTagsUpdate, AgentsSidebarPreferences, AgentsSidebarProjectVisibility,
    AgentsSmartViewsSnapshot, AgentsSmartViewsUpdate, AgentsViewFilters, AgentsViewPreferences,
    AgentsViewPreferencesNotifyPayload, AgentsViewPreferencesStoreErrorKind,
    AgentsViewPreferencesUpdate, BackendKind, BuiltInSmartViewId, CloseAgentPayload,
    CommandErrorCode, CommandErrorPayload, DeleteSessionPayload, Envelope, FrameKind,
    HostBootstrapPayload, HostFilterId, LOCAL_HOST_ID, NewAgentPayload, ProjectCreatePayload,
    ProjectNotifyPayload, SessionId, SetAgentGroupsPayload, SetAgentPinsPayload,
    SetAgentTagsPayload, SetAgentsSmartViewsPayload, SetAgentsViewPreferencesPayload, SmartView,
    SmartViewId, SpawnAgentParams, SpawnAgentPayload, StreamPath, UserSmartViewId, read_envelope,
    write_envelope,
};

async fn connect_host(host: server::HostHandle) -> (client::Connection, HostBootstrapPayload) {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig::current();
    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake failed");
        if let Err(err) = server::run_connection(conn, host).await {
            eprintln!("server connection loop failed: {err:?}");
        }
    });

    let mut client = client::connect(&client_config, client_stream)
        .await
        .expect("client handshake failed");
    let env = client
        .next_event()
        .await
        .expect("initial host bootstrap read failed")
        .expect("connection closed before initial host bootstrap");
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap = env.parse_payload().expect("parse HostBootstrapPayload");
    (client, bootstrap)
}

async fn send_set_agents_view_preferences(
    client: &mut client::Connection,
    update: AgentsViewPreferencesUpdate,
) {
    let payload = SetAgentsViewPreferencesPayload { update };
    send_host_payload(client, FrameKind::SetAgentsViewPreferences, &payload).await;
}

async fn send_set_agents_smart_views(
    client: &mut client::Connection,
    update: AgentsSmartViewsUpdate,
) {
    let payload = SetAgentsSmartViewsPayload { update };
    send_host_payload(client, FrameKind::SetAgentsSmartViews, &payload).await;
}

async fn send_set_agent_tags(client: &mut client::Connection, update: AgentTagsUpdate) {
    let payload = SetAgentTagsPayload { update };
    send_host_payload(client, FrameKind::SetAgentTags, &payload).await;
}

async fn send_set_agent_pins(client: &mut client::Connection, update: AgentPinsUpdate) {
    let payload = SetAgentPinsPayload { update };
    send_host_payload(client, FrameKind::SetAgentPins, &payload).await;
}

async fn send_set_agent_groups(client: &mut client::Connection, update: AgentGroupsUpdate) {
    let payload = SetAgentGroupsPayload { update };
    send_host_payload(client, FrameKind::SetAgentGroups, &payload).await;
}

async fn send_host_payload<T: serde::Serialize>(
    client: &mut client::Connection,
    kind: FrameKind,
    payload: &T,
) {
    let host_stream = single_host_stream(client);
    let seq = client
        .outgoing_seq
        .get(&host_stream)
        .copied()
        .expect("missing host stream sequence counter");
    let envelope = Envelope::from_payload(host_stream.clone(), kind, seq, payload)
        .expect("serialize host payload");
    client.outgoing_seq.insert(host_stream, seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write host payload");
}

fn single_host_stream(client: &client::Connection) -> StreamPath {
    let mut host_streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let host_stream = host_streams
        .next()
        .cloned()
        .expect("client should have a host stream");
    assert!(
        host_streams.next().is_none(),
        "client should have exactly one host stream"
    );
    host_stream
}

fn built_in_id(id: BuiltInSmartViewId) -> SmartViewId {
    SmartViewId::BuiltIn(id)
}

fn user_id(id: &str) -> SmartViewId {
    SmartViewId::User(UserSmartViewId(id.to_owned()))
}

fn user_view_id(view: &SmartView) -> UserSmartViewId {
    match &view.id {
        SmartViewId::User(id) => id.clone(),
        SmartViewId::BuiltIn(_) => panic!("expected user smart view id"),
    }
}

fn assert_built_in_smart_views(snapshot: &AgentsSmartViewsSnapshot) {
    assert_eq!(snapshot.built_in.len(), 3);
    assert_eq!(
        snapshot
            .built_in
            .iter()
            .map(|view| view.id.clone())
            .collect::<Vec<_>>(),
        vec![
            built_in_id(BuiltInSmartViewId::All),
            built_in_id(BuiltInSmartViewId::Active),
            built_in_id(BuiltInSmartViewId::FailedTerminated),
        ]
    );
    let all = &snapshot.built_in[0];
    assert_eq!(all.name, "All");
    assert_eq!(all.filters, AgentsViewFilters::default());
    assert_eq!(all.sort_mode, AgentSortMode::ManualThenActivity);
    assert_eq!(all.group_mode, AgentGroupMode::Flat);
    assert!(!all.hide_finished);

    let active = &snapshot.built_in[1];
    assert_eq!(active.name, "Active");
    assert_eq!(
        active.filters.statuses,
        vec![
            AgentStatusFilter::Initializing,
            AgentStatusFilter::Thinking,
            AgentStatusFilter::Compacting,
        ]
    );
    assert!(active.hide_finished);

    let failed = &snapshot.built_in[2];
    assert_eq!(failed.name, "Failed / terminated");
    assert_eq!(failed.filters.statuses, vec![AgentStatusFilter::Terminated]);
    assert!(!failed.hide_finished);
}

fn saved_view_filters() -> AgentsViewFilters {
    AgentsViewFilters {
        host_ids: vec![HostFilterId(LOCAL_HOST_ID.to_owned())],
        project_ids: vec![],
        statuses: vec![AgentStatusFilter::Idle],
        backends: vec![BackendKind::Codex, BackendKind::Claude],
        origins: vec![protocol::AgentOrigin::Workflow, protocol::AgentOrigin::User],
        tags: vec![],
    }
}

async fn expect_raw_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    loop {
        let envelope =
            match tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut client.reader))
                .await
            {
                Ok(Ok(Some(envelope))) => envelope,
                Ok(Ok(None)) => panic!("connection closed before {context}"),
                Ok(Err(err)) => panic!("read envelope failed before {context}: {err:?}"),
                Err(_) => panic!("timed out waiting for {context}"),
            };
        client
            .incoming_seq
            .validate(&envelope.stream, envelope.seq, envelope.kind)
            .expect("incoming sequence should be valid");
        if fixture::is_builtin_team_custom_agent_notify(&envelope) {
            continue;
        }
        if envelope.kind == kind {
            return envelope;
        }
        if envelope.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = envelope.parse_payload().expect("CommandError");
            panic!("unexpected CommandError before {context}: {error:?}");
        }
    }
}

async fn expect_preferences_notify(
    client: &mut client::Connection,
    context: &str,
) -> AgentsViewPreferencesNotifyPayload {
    let env = expect_raw_kind(client, FrameKind::AgentsViewPreferencesNotify, context).await;
    env.parse_payload()
        .expect("parse AgentsViewPreferencesNotifyPayload")
}

async fn assert_no_agent_closed_for(
    client: &mut client::Connection,
    agent_ids: &[AgentId],
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    loop {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed while checking {context}"),
            Ok(Err(err)) => panic!("next_event failed while checking {context}: {err:?}"),
            Err(_) => break,
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("CommandError");
            panic!("unexpected CommandError while checking {context}: {error:?}");
        }
        if env.kind == FrameKind::AgentClosed {
            let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
            assert!(
                !agent_ids.contains(&closed.agent_id),
                "{context}: delete-group must ungroup, not close agent {}",
                closed.agent_id
            );
        }
    }
}

async fn expect_non_empty_group_notify(
    client: &mut client::Connection,
    context: &str,
) -> AgentsViewPreferencesNotifyPayload {
    loop {
        let notify = expect_preferences_notify(client, context).await;
        if !notify.snapshot.groups.groups.is_empty() {
            return notify;
        }
    }
}

async fn expect_preferences_notify_where(
    client: &mut client::Connection,
    context: &str,
    mut predicate: impl FnMut(&AgentsViewPreferencesNotifyPayload) -> bool,
) -> AgentsViewPreferencesNotifyPayload {
    loop {
        let notify = expect_preferences_notify(client, context).await;
        if predicate(&notify) {
            return notify;
        }
    }
}

async fn set_active_preferences_query(
    client: &mut client::Connection,
    filters: AgentsViewFilters,
    sort_mode: AgentSortMode,
    group_mode: AgentGroupMode,
    hide_finished: bool,
) -> AgentsViewPreferencesNotifyPayload {
    send_set_agents_view_preferences(client, AgentsViewPreferencesUpdate::SetFilters { filters })
        .await;
    expect_preferences_notify(client, "set filters notify").await;
    send_set_agents_view_preferences(
        client,
        AgentsViewPreferencesUpdate::SetSortMode { sort_mode },
    )
    .await;
    expect_preferences_notify(client, "set sort notify").await;
    send_set_agents_view_preferences(
        client,
        AgentsViewPreferencesUpdate::SetGroupMode { group_mode },
    )
    .await;
    expect_preferences_notify(client, "set group notify").await;
    send_set_agents_view_preferences(
        client,
        AgentsViewPreferencesUpdate::SetHideFinished { hide_finished },
    )
    .await;
    expect_preferences_notify(client, "set hide finished notify").await
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    let env = expect_client_kind(client, FrameKind::CommandError, context).await;
    env.parse_payload().expect("parse CommandErrorPayload")
}

async fn expect_client_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == kind {
            return env;
        }
    }
}

async fn spawn_agent_for_order(client: &mut client::Connection) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("ordered-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/agents-view-preferences".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");

    let env = expect_client_kind(client, FrameKind::NewAgent, "NewAgent").await;
    env.parse_payload().expect("parse NewAgentPayload")
}

async fn spawn_agent_for_tags(
    client: &mut client::Connection,
    backend_kind: BackendKind,
    project_id: Option<protocol::ProjectId>,
    prompt: &str,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("tagged-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/agents-view-tags".to_owned()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent for tags");

    let env = expect_client_kind(client, FrameKind::NewAgent, "NewAgent for tags").await;
    env.parse_payload().expect("parse NewAgentPayload")
}

async fn spawn_agent_with_parent(
    client: &mut client::Connection,
    name: &str,
    parent_agent_id: Option<AgentId>,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/agents-view-groups".to_owned()],
                prompt: name.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent for groups");

    let env = expect_client_kind(client, FrameKind::NewAgent, "NewAgent for groups").await;
    env.parse_payload().expect("parse NewAgentPayload")
}

async fn expect_agent_start_payload(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
) -> AgentStartPayload {
    loop {
        let env = expect_client_kind_any_agent_start(client, "ordered agent start").await;
        if env.stream != *agent_stream {
            continue;
        }
        if env.kind == FrameKind::AgentStart {
            return env.parse_payload().expect("parse AgentStartPayload");
        }
        let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
        for event in bootstrap.events {
            if let AgentBootstrapEvent::AgentStart(start) = event {
                return start;
            }
        }
    }
}

fn local_session_target(session_id: SessionId) -> AgentAnnotationTarget {
    AgentAnnotationTarget::Session {
        host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
        session_id,
    }
}

fn local_transient_target(agent_id: AgentId) -> AgentAnnotationTarget {
    AgentAnnotationTarget::TransientAgent {
        host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
        agent_id,
    }
}

async fn create_manual_tag(
    client: &mut client::Connection,
    name: &str,
    color: Option<&str>,
) -> AgentManualTagId {
    send_set_agent_tags(
        client,
        AgentTagsUpdate::CreateTag {
            name: name.to_owned(),
            color: color.map(|color| AgentTagColor(color.to_owned())),
        },
    )
    .await;
    let notify = expect_preferences_notify_where(client, "create tag notify", |notify| {
        !notify.snapshot.tags.manual.is_empty()
    })
    .await;
    notify
        .snapshot
        .tags
        .manual
        .last()
        .expect("created manual tag")
        .id
        .clone()
}

async fn close_agent(client: &mut client::Connection, agent_stream: &StreamPath) {
    let seq = client.outgoing_seq.get(agent_stream).copied().unwrap_or(0);
    let envelope = Envelope::from_payload(
        agent_stream.clone(),
        FrameKind::CloseAgent,
        seq,
        &CloseAgentPayload {},
    )
    .expect("serialize close agent");
    client.outgoing_seq.insert(agent_stream.clone(), seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write close agent");
}

async fn create_project(client: &mut client::Connection, name: &str) -> protocol::Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: vec![protocol::ProjectRootPath(format!("/tmp/{name}"))],
        })
        .await
        .expect("create project");
    let env = expect_client_kind(client, FrameKind::ProjectNotify, "project create notify").await;
    match env
        .parse_payload::<ProjectNotifyPayload>()
        .expect("parse project notify")
    {
        ProjectNotifyPayload::Upsert { project } => project,
        ProjectNotifyPayload::Delete { .. } => panic!("unexpected project delete notify"),
    }
}

async fn expect_client_kind_any_agent_start(
    client: &mut client::Connection,
    context: &str,
) -> Envelope {
    loop {
        let env =
            match tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut client.reader))
                .await
            {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!("connection closed before {context}"),
                Ok(Err(err)) => panic!("read envelope failed before {context}: {err:?}"),
                Err(_) => panic!("timed out waiting for {context}"),
            };
        client
            .incoming_seq
            .validate(&env.stream, env.seq, env.kind)
            .expect("incoming sequence should be valid");
        if env.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
            client.outgoing_seq.insert(payload.instance_stream, 0);
            continue;
        }
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("CommandError");
            panic!("unexpected CommandError before {context}: {error:?}");
        }
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if matches!(env.kind, FrameKind::AgentStart | FrameKind::AgentBootstrap) {
            return env;
        }
    }
}

async fn expect_promoted_manual_assignment(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    tag_id: &AgentManualTagId,
) -> (SessionId, AgentsViewPreferencesNotifyPayload) {
    let mut session_id = None;
    let mut latest_notify = None;
    loop {
        let env =
            match tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut client.reader))
                .await
            {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!("connection closed before promoted assignment"),
                Ok(Err(err)) => panic!("read envelope failed before promoted assignment: {err:?}"),
                Err(_) => panic!("timed out waiting for promoted assignment"),
            };
        client
            .incoming_seq
            .validate(&env.stream, env.seq, env.kind)
            .expect("incoming sequence should be valid");
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::AgentStart if env.stream == *agent_stream => {
                let start: AgentStartPayload =
                    env.parse_payload().expect("parse AgentStartPayload");
                session_id = start.session_id;
            }
            FrameKind::AgentBootstrap if env.stream == *agent_stream => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse AgentBootstrapPayload");
                for event in bootstrap.events {
                    if let AgentBootstrapEvent::AgentStart(start) = event {
                        session_id = start.session_id;
                    }
                }
            }
            FrameKind::AgentsViewPreferencesNotify => {
                latest_notify = Some(
                    env.parse_payload::<AgentsViewPreferencesNotifyPayload>()
                        .expect("parse AgentsViewPreferencesNotifyPayload"),
                );
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().expect("CommandError");
                panic!("unexpected CommandError before promoted assignment: {error:?}");
            }
            _ => {}
        }

        if let (Some(session_id), Some(notify)) = (session_id.clone(), latest_notify.as_ref()) {
            let target = local_session_target(session_id.clone());
            if notify
                .snapshot
                .tags
                .manual_assignments
                .iter()
                .any(|assignment| {
                    assignment.target == target && assignment.tag_ids == vec![tag_id.clone()]
                })
            {
                return (session_id, notify.clone());
            }
        }
    }
}

async fn expect_promoted_group_assignment(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    group_id: &AgentGroupId,
) -> (SessionId, AgentsViewPreferencesNotifyPayload) {
    let mut session_id = None;
    let mut latest_notify = None;
    loop {
        let env =
            match tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut client.reader))
                .await
            {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!("connection closed before promoted group assignment"),
                Ok(Err(err)) => {
                    panic!("read envelope failed before promoted group assignment: {err:?}")
                }
                Err(_) => panic!("timed out waiting for promoted group assignment"),
            };
        client
            .incoming_seq
            .validate(&env.stream, env.seq, env.kind)
            .expect("incoming sequence should be valid");
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::AgentStart if env.stream == *agent_stream => {
                let start: AgentStartPayload =
                    env.parse_payload().expect("parse AgentStartPayload");
                session_id = start.session_id;
            }
            FrameKind::AgentBootstrap if env.stream == *agent_stream => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse AgentBootstrapPayload");
                for event in bootstrap.events {
                    if let AgentBootstrapEvent::AgentStart(start) = event {
                        session_id = start.session_id;
                    }
                }
            }
            FrameKind::AgentsViewPreferencesNotify => {
                latest_notify = Some(
                    env.parse_payload::<AgentsViewPreferencesNotifyPayload>()
                        .expect("parse AgentsViewPreferencesNotifyPayload"),
                );
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().expect("CommandError");
                panic!("unexpected CommandError before promoted group assignment: {error:?}");
            }
            _ => {}
        }

        if let (Some(session_id), Some(notify)) = (session_id.clone(), latest_notify.as_ref()) {
            let target = local_session_target(session_id.clone());
            if notify
                .snapshot
                .groups
                .assignments
                .iter()
                .any(|assignment| assignment.target == target && assignment.group_id == *group_id)
            {
                return (session_id, notify.clone());
            }
        }
    }
}

#[tokio::test]
async fn agents_view_preferences_non_primary_host_omits_preferences_and_rejects_writes() {
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        agents_view_preferences_primary: false,
        ..server::HostRuntimeConfig::default()
    })
    .await;
    assert_eq!(fixture.bootstrap.agents_view_preferences, None);

    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetManualOrder {
            manual_order: vec![AgentOrderKey::Session {
                session_id: SessionId(String::new()),
            }],
        },
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "non-primary preferences error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsViewPreferences);
    assert_eq!(error.operation, "set_agents_view_preferences");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("owned by the primary local host"),
        "unexpected error message: {}",
        error.message
    );
    assert!(
        !fixture
            .store_dir()
            .join("agents_view_preferences.json")
            .exists(),
        "non-primary host must not create a competing preferences store"
    );

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Remote view".to_owned(),
        },
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "non-primary smart views error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert_eq!(error.operation, "set_agents_smart_views");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("owned by the primary local host"),
        "unexpected error message: {}",
        error.message
    );
    assert!(
        !fixture
            .store_dir()
            .join("agents_view_preferences.json")
            .exists(),
        "non-primary host must not create a competing smart view store"
    );

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::CreateTag {
            name: "Remote tag".to_owned(),
            color: None,
        },
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "non-primary tags error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentTags);
    assert_eq!(error.operation, "set_agent_tags");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("owned by the primary local host"),
        "unexpected error message: {}",
        error.message
    );

    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Pin {
            target: local_transient_target(AgentId("remote-pin".to_owned())),
        },
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "non-primary pins error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentPins);
    assert_eq!(error.operation, "set_agent_pins");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("owned by the primary local host"),
        "unexpected error message: {}",
        error.message
    );

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Remote group".to_owned(),
            targets: vec![local_transient_target(AgentId("remote-group".to_owned()))],
        },
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "non-primary groups error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentGroups);
    assert_eq!(error.operation, "set_agent_groups");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("owned by the primary local host"),
        "unexpected error message: {}",
        error.message
    );
}

#[tokio::test]
async fn agents_view_preferences_update_notifies_and_persists_to_bootstrap() {
    let mut fixture = Fixture::new().await;
    let bootstrap_snapshot = fixture
        .bootstrap
        .agents_view_preferences
        .as_ref()
        .expect("primary bootstrap preferences");
    assert_eq!(
        bootstrap_snapshot.preferences,
        AgentsViewPreferences::default()
    );
    assert_eq!(bootstrap_snapshot.load_error, None);
    assert_built_in_smart_views(&bootstrap_snapshot.smart_views);
    assert!(bootstrap_snapshot.smart_views.user.is_empty());
    assert_eq!(
        bootstrap_snapshot.smart_views.active_view_id,
        Some(built_in_id(BuiltInSmartViewId::All))
    );
    assert!(bootstrap_snapshot.tags.manual.is_empty());
    assert!(bootstrap_snapshot.tags.manual_assignments.is_empty());
    assert!(bootstrap_snapshot.tags.system.is_empty());
    assert!(bootstrap_snapshot.tags.system_assignments.is_empty());
    assert!(bootstrap_snapshot.pins.pinned.is_empty());
    assert_eq!(
        bootstrap_snapshot.sidebar,
        AgentsSidebarPreferences::default()
    );

    let filters = AgentsViewFilters {
        host_ids: vec![HostFilterId(LOCAL_HOST_ID.to_owned())],
        project_ids: vec![],
        statuses: vec![protocol::AgentStatusFilter::Idle],
        backends: vec![BackendKind::Codex, BackendKind::Claude],
        origins: vec![protocol::AgentOrigin::Workflow, protocol::AgentOrigin::User],
        tags: vec![],
    };
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetFilters { filters },
    )
    .await;

    let notify = expect_preferences_notify(&mut fixture.client, "preferences notify").await;
    assert!(notify.snapshot.load_error.is_none());
    assert_eq!(
        notify.snapshot.preferences.filters.host_ids,
        vec![HostFilterId(LOCAL_HOST_ID.to_owned())]
    );
    assert_eq!(
        notify.snapshot.preferences.filters.backends,
        vec![BackendKind::Claude, BackendKind::Codex]
    );
    assert_eq!(
        notify.snapshot.preferences.filters.origins,
        vec![protocol::AgentOrigin::User, protocol::AgentOrigin::Workflow]
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert_eq!(
        fresh_bootstrap
            .agents_view_preferences
            .expect("fresh bootstrap preferences")
            .preferences,
        notify.snapshot.preferences
    );
}

#[tokio::test]
async fn agents_sidebar_preferences_update_notifies_and_persists_to_bootstrap() {
    let mut fixture = Fixture::new().await;
    let sidebar = AgentsSidebarPreferences {
        hide_inactive: true,
        hide_sub_agents: true,
        project_visibility: AgentsSidebarProjectVisibility::CurrentProjectOnly,
    };

    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetSidebarPreferences {
            sidebar: sidebar.clone(),
        },
    )
    .await;

    let notify = expect_preferences_notify(&mut fixture.client, "sidebar preferences notify").await;
    assert_eq!(notify.snapshot.sidebar, sidebar);
    assert_eq!(
        notify.snapshot.preferences,
        AgentsViewPreferences::default(),
        "sidebar update must not mutate existing view query preferences"
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert_eq!(
        fresh_bootstrap
            .agents_view_preferences
            .expect("fresh bootstrap preferences")
            .sidebar,
        sidebar
    );
}

#[tokio::test]
async fn corrupt_agents_view_preferences_store_does_not_block_bootstrap_and_clears_on_reset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let preferences_path = dir.path().join("agents_view_preferences.json");
    std::fs::write(&preferences_path, "not json").expect("write corrupt preferences store");
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");

    let (mut client, bootstrap) = connect_host(host).await;
    let snapshot = bootstrap
        .agents_view_preferences
        .expect("primary bootstrap preferences");
    assert_eq!(snapshot.preferences, AgentsViewPreferences::default());
    assert_eq!(
        snapshot.load_error.as_ref().map(|error| error.kind),
        Some(AgentsViewPreferencesStoreErrorKind::Corrupt)
    );
    assert_built_in_smart_views(&snapshot.smart_views);
    assert!(snapshot.smart_views.user.is_empty());
    assert_eq!(
        snapshot.smart_views.active_view_id,
        Some(built_in_id(BuiltInSmartViewId::All))
    );
    assert!(snapshot.tags.manual.is_empty());
    assert!(snapshot.tags.manual_assignments.is_empty());
    assert!(snapshot.pins.pinned.is_empty());
    assert!(snapshot.tags.manual.is_empty());
    assert!(snapshot.tags.manual_assignments.is_empty());
    assert!(snapshot.pins.pinned.is_empty());

    send_set_agents_view_preferences(&mut client, AgentsViewPreferencesUpdate::Reset).await;
    let notify = expect_preferences_notify(&mut client, "reset preferences notify").await;
    assert_eq!(
        notify.snapshot.preferences,
        AgentsViewPreferences::default()
    );
    assert!(notify.snapshot.load_error.is_none());
    assert_built_in_smart_views(&notify.snapshot.smart_views);
    assert!(notify.snapshot.smart_views.user.is_empty());
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(built_in_id(BuiltInSmartViewId::All))
    );
    assert!(notify.snapshot.tags.manual.is_empty());
    assert!(notify.snapshot.tags.manual_assignments.is_empty());
    assert!(notify.snapshot.pins.pinned.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_agent_tags_manual_lifecycle_persists_and_deletes_assignments() {
    let mut fixture = Fixture::new().await;
    let agent = spawn_agent_for_tags(&mut fixture.client, BackendKind::Claude, None, "hello").await;
    let start = expect_agent_start_payload(&mut fixture.client, &agent.instance_stream).await;
    let session_id = start.session_id.expect("agent start session id");
    let target = local_session_target(session_id.clone());

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::CreateTag {
            name: "  Needs Review  ".to_owned(),
            color: Some(AgentTagColor("#ffcc00".to_owned())),
        },
    )
    .await;
    let notify =
        expect_preferences_notify_where(&mut fixture.client, "create manual tag", |notify| {
            notify.snapshot.tags.manual.len() == 1
        })
        .await;
    assert_eq!(notify.snapshot.tags.manual.len(), 1);
    let tag_id = notify.snapshot.tags.manual[0].id.clone();
    assert_eq!(tag_id, AgentManualTagId("needs-review".to_owned()));
    assert_eq!(notify.snapshot.tags.manual[0].name, "Needs Review");
    assert_eq!(
        notify.snapshot.tags.manual[0].color,
        Some(AgentTagColor("#ffcc00".to_owned()))
    );

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::AssignTag {
            target: target.clone(),
            tag_id: tag_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "assign manual tag").await;
    assert_eq!(
        notify.snapshot.tags.manual_assignments,
        vec![protocol::AgentManualTagAssignment {
            target: target.clone(),
            tag_ids: vec![tag_id.clone()],
        }]
    );

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::RemoveTag {
            target: target.clone(),
            tag_id: tag_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "remove manual tag").await;
    assert!(notify.snapshot.tags.manual_assignments.is_empty());

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::AssignTag {
            target: target.clone(),
            tag_id: tag_id.clone(),
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "reassign manual tag").await;

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::RenameTag {
            tag_id: tag_id.clone(),
            name: "Renamed Tag".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "rename manual tag").await;
    assert_eq!(notify.snapshot.tags.manual[0].name, "Renamed Tag");

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::SetTagColor {
            tag_id: tag_id.clone(),
            color: Some(AgentTagColor("#11223344".to_owned())),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "recolor manual tag").await;
    assert_eq!(
        notify.snapshot.tags.manual[0].color,
        Some(AgentTagColor("#11223344".to_owned()))
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_snapshot = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences");
    assert_eq!(fresh_snapshot.tags.manual, notify.snapshot.tags.manual);
    assert_eq!(
        fresh_snapshot.tags.manual_assignments,
        notify.snapshot.tags.manual_assignments
    );

    send_set_agent_tags(&mut fixture.client, AgentTagsUpdate::DeleteTag { tag_id }).await;
    let notify = expect_preferences_notify(&mut fixture.client, "delete manual tag").await;
    assert!(notify.snapshot.tags.manual.is_empty());
    assert!(notify.snapshot.tags.manual_assignments.is_empty());

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_snapshot = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences after delete");
    assert!(fresh_snapshot.tags.manual.is_empty());
    assert!(fresh_snapshot.tags.manual_assignments.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_agent_tags_delete_strips_filter_refs() {
    let mut fixture = Fixture::new().await;
    let tag_id = create_manual_tag(&mut fixture.client, "Archive", None).await;
    let remaining_system_tag =
        AgentTagRef::System(AgentSystemTagId("system:backend:codex".to_owned()));
    let filters = AgentsViewFilters {
        tags: vec![
            AgentTagRef::Manual(tag_id.clone()),
            remaining_system_tag.clone(),
        ],
        ..AgentsViewFilters::default()
    };

    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetFilters {
            filters: filters.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "set tag filters").await;
    assert_eq!(notify.snapshot.preferences.filters.tags, filters.tags);

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Tagged Archive".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "save tagged smart view").await;
    assert_eq!(notify.snapshot.smart_views.user.len(), 1);
    assert_eq!(
        notify.snapshot.smart_views.user[0].filters.tags,
        filters.tags
    );

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::DeleteTag {
            tag_id: tag_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "delete filtered tag").await;
    assert!(notify.snapshot.tags.manual.is_empty());
    assert_eq!(
        notify.snapshot.preferences.filters.tags,
        vec![remaining_system_tag.clone()]
    );
    assert_eq!(
        notify.snapshot.smart_views.user[0].filters.tags,
        vec![remaining_system_tag]
    );
    assert!(
        !notify
            .snapshot
            .preferences
            .filters
            .tags
            .contains(&AgentTagRef::Manual(tag_id.clone()))
    );
    assert!(!notify.snapshot.smart_views.user.iter().any(|view| {
        view.filters
            .tags
            .contains(&AgentTagRef::Manual(tag_id.clone()))
    }));
}

#[tokio::test]
async fn agents_view_preferences_agent_tags_filters_group_mode_and_system_tags_are_snapshotted() {
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, "tags-project").await;
    let agent = spawn_agent_for_tags(
        &mut fixture.client,
        BackendKind::Codex,
        Some(project.id.clone()),
        "project agent",
    )
    .await;
    let start = expect_agent_start_payload(&mut fixture.client, &agent.instance_stream).await;
    let session_id = start.session_id.expect("agent start session id");
    let target = local_session_target(session_id);
    let tag_id = create_manual_tag(&mut fixture.client, "Important", None).await;

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::AssignTag {
            target: target.clone(),
            tag_id: tag_id.clone(),
        },
    )
    .await;
    let notify =
        expect_preferences_notify(&mut fixture.client, "assign tag before filtering").await;
    assert_eq!(
        notify.snapshot.tags.manual_assignments,
        vec![protocol::AgentManualTagAssignment {
            target: target.clone(),
            tag_ids: vec![tag_id.clone()],
        }]
    );

    let system_ids = notify
        .snapshot
        .tags
        .system_assignments
        .iter()
        .find(|assignment| assignment.target == target)
        .expect("system assignment for tagged agent")
        .tag_ids
        .clone();
    assert!(system_ids.contains(&AgentSystemTagId("system:origin:user".to_owned())));
    assert!(system_ids.contains(&AgentSystemTagId("system:backend:codex".to_owned())));
    assert!(system_ids.contains(&AgentSystemTagId(format!(
        "system:project:{}",
        project.id.0
    ))));
    let system_labels = notify
        .snapshot
        .tags
        .system
        .iter()
        .map(|descriptor| (descriptor.id.clone(), descriptor.name.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        system_labels.get(&AgentSystemTagId("system:origin:user".to_owned())),
        Some(&"User".to_owned())
    );
    assert_eq!(
        system_labels.get(&AgentSystemTagId("system:backend:codex".to_owned())),
        Some(&"Codex".to_owned())
    );
    assert_eq!(
        system_labels.get(&AgentSystemTagId(format!(
            "system:project:{}",
            project.id.0
        ))),
        Some(&project.name)
    );

    let filters = AgentsViewFilters {
        tags: vec![
            AgentTagRef::Manual(tag_id.clone()),
            AgentTagRef::System(AgentSystemTagId("system:backend:codex".to_owned())),
        ],
        ..AgentsViewFilters::default()
    };
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetFilters {
            filters: filters.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "tag filter notify").await;
    assert_eq!(notify.snapshot.preferences.filters.tags, filters.tags);

    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetGroupMode {
            group_mode: AgentGroupMode::Tag,
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "tag group notify").await;
    assert_eq!(notify.snapshot.preferences.group_mode, AgentGroupMode::Tag);
}

#[tokio::test]
async fn agents_view_preferences_agent_pins_canonicalize_and_persist_session_targets() {
    let mut fixture = Fixture::new().await;
    let agent =
        spawn_agent_for_tags(&mut fixture.client, BackendKind::Claude, None, "pin me").await;
    let start = expect_agent_start_payload(&mut fixture.client, &agent.instance_stream).await;
    let session_id = start.session_id.expect("agent start session id");
    let session_target = local_session_target(session_id.clone());
    let transient_target = local_transient_target(agent.agent_id.clone());

    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Pin {
            target: session_target.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify_where(&mut fixture.client, "pin session", |notify| {
        notify.snapshot.pins.pinned == vec![session_target.clone()]
    })
    .await;
    assert_eq!(notify.snapshot.pins.pinned, vec![session_target.clone()]);

    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Pin {
            target: transient_target.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "pin duplicate transient").await;
    assert_eq!(notify.snapshot.pins.pinned, vec![session_target.clone()]);

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert_eq!(
        fresh_bootstrap
            .agents_view_preferences
            .expect("fresh bootstrap preferences")
            .pins
            .pinned,
        vec![session_target.clone()]
    );

    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Unpin {
            target: transient_target,
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "unpin transient").await;
    assert!(notify.snapshot.pins.pinned.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_agent_groups_lifecycle_single_membership_and_persistence() {
    let mut fixture = Fixture::new().await;
    let first =
        spawn_agent_for_tags(&mut fixture.client, BackendKind::Claude, None, "group one").await;
    let first_start = expect_agent_start_payload(&mut fixture.client, &first.instance_stream).await;
    let second =
        spawn_agent_for_tags(&mut fixture.client, BackendKind::Claude, None, "group two").await;
    let second_start =
        expect_agent_start_payload(&mut fixture.client, &second.instance_stream).await;

    let first_target = local_session_target(first_start.session_id.expect("first session id"));
    let second_target = local_session_target(second_start.session_id.expect("second session id"));

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "  Review Pair  ".to_owned(),
            targets: vec![first_target.clone(), second_target.clone()],
        },
    )
    .await;
    let notify = expect_non_empty_group_notify(&mut fixture.client, "create group notify").await;
    assert_eq!(notify.snapshot.groups.groups.len(), 1);
    let first_group_id = notify.snapshot.groups.groups[0].id.clone();
    assert_eq!(first_group_id, AgentGroupId("review-pair".to_owned()));
    assert_eq!(notify.snapshot.groups.groups[0].name, "Review Pair");
    assert_eq!(notify.snapshot.groups.assignments.len(), 2);
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id: first_group_id.clone(),
                target: first_target.clone(),
            })
    );
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id: first_group_id.clone(),
                target: second_target.clone(),
            })
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_groups = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences")
        .groups;
    assert_eq!(fresh_groups.groups, notify.snapshot.groups.groups);
    assert_eq!(fresh_groups.assignments, notify.snapshot.groups.assignments);

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Solo".to_owned(),
            targets: vec![second_target.clone()],
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "move to second group").await;
    let second_group_id = notify
        .snapshot
        .groups
        .groups
        .iter()
        .find(|group| group.name == "Solo")
        .expect("second group")
        .id
        .clone();
    assert_eq!(notify.snapshot.groups.assignments.len(), 2);
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id: first_group_id.clone(),
                target: first_target.clone(),
            }),
        "first target should stay in the first group"
    );
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id: second_group_id.clone(),
                target: second_target.clone(),
            }),
        "moving to another group enforces one group per target"
    );

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::MoveTargets {
            group_id: None,
            targets: vec![first_target.clone()],
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "ungroup first target").await;
    assert_eq!(notify.snapshot.groups.groups.len(), 1);
    assert_eq!(notify.snapshot.groups.groups[0].id, second_group_id);
    assert_eq!(
        notify.snapshot.groups.assignments,
        vec![AgentGroupAssignment {
            group_id: second_group_id.clone(),
            target: second_target.clone(),
        }],
        "empty groups auto-delete when their last member leaves"
    );

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::DeleteGroup {
            id: second_group_id,
        },
    )
    .await;
    let notify =
        expect_preferences_notify(&mut fixture.client, "delete group ungroup notify").await;
    assert!(notify.snapshot.groups.groups.is_empty());
    assert!(notify.snapshot.groups.assignments.is_empty());
    assert_no_agent_closed_for(
        &mut fixture.client,
        &[first.agent_id.clone(), second.agent_id.clone()],
        "delete group ungroup",
    )
    .await;
}

#[tokio::test]
async fn agents_view_preferences_agent_groups_moving_parent_moves_children() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_with_parent(&mut fixture.client, "parent group", None).await;
    let parent_start =
        expect_agent_start_payload(&mut fixture.client, &parent.instance_stream).await;
    let child = spawn_agent_with_parent(
        &mut fixture.client,
        "child group",
        Some(parent.agent_id.clone()),
    )
    .await;
    let child_start = expect_agent_start_payload(&mut fixture.client, &child.instance_stream).await;

    let parent_target = local_session_target(parent_start.session_id.expect("parent session id"));
    let child_target = local_session_target(child_start.session_id.expect("child session id"));

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Family".to_owned(),
            targets: vec![parent_target.clone()],
        },
    )
    .await;
    let notify = expect_non_empty_group_notify(&mut fixture.client, "parent group create").await;
    let group_id = notify.snapshot.groups.groups[0].id.clone();
    assert_eq!(notify.snapshot.groups.assignments.len(), 2);
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id: group_id.clone(),
                target: parent_target,
            })
    );
    assert!(
        notify
            .snapshot
            .groups
            .assignments
            .contains(&AgentGroupAssignment {
                group_id,
                target: child_target,
            })
    );
}

#[tokio::test]
async fn agents_view_preferences_agent_groups_promote_and_cleanup_targets() {
    let mut fixture = Fixture::new().await;
    let agent = spawn_agent_for_tags(
        &mut fixture.client,
        BackendKind::Claude,
        None,
        "group promote",
    )
    .await;
    let transient_target = local_transient_target(agent.agent_id.clone());

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Promotion".to_owned(),
            targets: vec![transient_target],
        },
    )
    .await;
    let create_notify =
        expect_non_empty_group_notify(&mut fixture.client, "create transient group").await;
    let group_id = create_notify.snapshot.groups.groups[0].id.clone();
    let already_promoted_session_id = create_notify
        .snapshot
        .groups
        .assignments
        .iter()
        .find(|assignment| assignment.group_id == group_id)
        .and_then(|assignment| match &assignment.target {
            AgentAnnotationTarget::Session { session_id, .. } => Some(session_id.clone()),
            AgentAnnotationTarget::TransientAgent { .. } => None,
        });
    let (session_id, promoted_notify) = if let Some(session_id) = already_promoted_session_id {
        (session_id, create_notify)
    } else {
        expect_promoted_group_assignment(&mut fixture.client, &agent.instance_stream, &group_id)
            .await
    };
    let session_target = local_session_target(session_id.clone());
    assert_eq!(
        promoted_notify.snapshot.groups.assignments,
        vec![AgentGroupAssignment {
            group_id: group_id.clone(),
            target: session_target,
        }]
    );

    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: session_id.clone(),
        })
        .await
        .expect("delete session");
    let notify = expect_preferences_notify(&mut fixture.client, "group session cleanup").await;
    assert!(notify.snapshot.groups.groups.is_empty());
    assert!(notify.snapshot.groups.assignments.is_empty());

    let failing = spawn_agent_for_tags(
        &mut fixture.client,
        BackendKind::Claude,
        None,
        "__mock_fail_spawn__ no group session",
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "sessionless new agent system tags").await;
    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Transient only".to_owned(),
            targets: vec![local_transient_target(failing.agent_id.clone())],
        },
    )
    .await;
    let notify =
        expect_preferences_notify(&mut fixture.client, "sessionless transient group").await;
    assert_eq!(notify.snapshot.groups.groups.len(), 1);
    close_agent(&mut fixture.client, &failing.instance_stream).await;
    let notify =
        expect_preferences_notify(&mut fixture.client, "sessionless group close cleanup").await;
    assert!(notify.snapshot.groups.groups.is_empty());
    assert!(notify.snapshot.groups.assignments.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_agent_groups_reject_invalid_updates() {
    let mut fixture = Fixture::new().await;

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "   ".to_owned(),
            targets: vec![local_transient_target(AgentId("agent-a".to_owned()))],
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "empty group name error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentGroups);
    assert_eq!(error.operation, "set_agent_groups");
    assert!(error.message.contains("name must not be empty"));

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::CreateGroup {
            name: "Empty targets".to_owned(),
            targets: Vec::new(),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "empty group targets error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentGroups);
    assert!(error.message.contains("targets must not be empty"));

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::MoveTargets {
            group_id: Some(AgentGroupId("missing".to_owned())),
            targets: vec![local_transient_target(AgentId("agent-a".to_owned()))],
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "unknown group move error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentGroups);
    assert!(error.message.contains("unknown agent group id"));

    send_set_agent_groups(
        &mut fixture.client,
        AgentGroupsUpdate::RenameGroup {
            id: AgentGroupId("bad id".to_owned()),
            name: "Bad".to_owned(),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "invalid group id error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentGroups);
    assert!(error.message.contains("must be sanitized"));
}

#[tokio::test]
async fn agents_view_preferences_agent_annotations_promote_drop_and_session_delete_cleanup() {
    let mut fixture = Fixture::new().await;
    let tag_id = create_manual_tag(&mut fixture.client, "Cleanup", None).await;
    let agent =
        spawn_agent_for_tags(&mut fixture.client, BackendKind::Claude, None, "cleanup").await;
    let transient_target = local_transient_target(agent.agent_id.clone());

    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::AssignTag {
            target: transient_target.clone(),
            tag_id: tag_id.clone(),
        },
    )
    .await;
    let (session_id, promoted_notify) =
        expect_promoted_manual_assignment(&mut fixture.client, &agent.instance_stream, &tag_id)
            .await;
    let session_target = local_session_target(session_id.clone());
    assert_eq!(
        promoted_notify.snapshot.tags.manual_assignments,
        vec![protocol::AgentManualTagAssignment {
            target: session_target.clone(),
            tag_ids: vec![tag_id.clone()],
        }]
    );

    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Pin {
            target: session_target.clone(),
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "pin before delete").await;

    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: session_id.clone(),
        })
        .await
        .expect("delete session");
    let notify = expect_preferences_notify(&mut fixture.client, "session delete cleanup").await;
    assert!(notify.snapshot.tags.manual_assignments.is_empty());
    assert!(notify.snapshot.pins.pinned.is_empty());
    assert_eq!(notify.snapshot.tags.manual[0].id, tag_id);

    let failing = spawn_agent_for_tags(
        &mut fixture.client,
        BackendKind::Claude,
        None,
        "__mock_fail_spawn__ no session",
    )
    .await;
    let transient = local_transient_target(failing.agent_id.clone());
    expect_preferences_notify(&mut fixture.client, "sessionless new agent system tags").await;
    let cleanup_tag_id = notify.snapshot.tags.manual[0].id.clone();
    send_set_agent_tags(
        &mut fixture.client,
        AgentTagsUpdate::AssignTag {
            target: transient.clone(),
            tag_id: cleanup_tag_id,
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "sessionless transient tag").await;
    send_set_agent_pins(
        &mut fixture.client,
        AgentPinsUpdate::Pin {
            target: transient.clone(),
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "sessionless transient pin").await;
    close_agent(&mut fixture.client, &failing.instance_stream).await;
    let notify = expect_preferences_notify(&mut fixture.client, "sessionless close cleanup").await;
    assert!(notify.snapshot.tags.manual_assignments.is_empty());
    assert!(notify.snapshot.pins.pinned.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_smart_views_save_current_persists_user_view() {
    let mut fixture = Fixture::new().await;
    let query_notify = set_active_preferences_query(
        &mut fixture.client,
        saved_view_filters(),
        AgentSortMode::NameAsc,
        AgentGroupMode::Backend,
        true,
    )
    .await;
    let expected_preferences = query_notify.snapshot.preferences;

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "  Focused Work  ".to_owned(),
        },
    )
    .await;

    let notify = expect_preferences_notify(&mut fixture.client, "save current notify").await;
    assert_built_in_smart_views(&notify.snapshot.smart_views);
    assert_eq!(notify.snapshot.smart_views.active_view_id, None);
    assert_eq!(notify.snapshot.smart_views.user.len(), 1);
    let saved = &notify.snapshot.smart_views.user[0];
    assert_eq!(saved.id, user_id("focused-work"));
    assert_eq!(saved.name, "Focused Work");
    assert_eq!(saved.filters, expected_preferences.filters);
    assert_eq!(saved.sort_mode, expected_preferences.sort_mode);
    assert_eq!(saved.group_mode, expected_preferences.group_mode);
    assert_eq!(saved.hide_finished, expected_preferences.hide_finished);

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_snapshot = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences");
    assert_built_in_smart_views(&fresh_snapshot.smart_views);
    assert_eq!(fresh_snapshot.smart_views.user, vec![saved.clone()]);
    assert_eq!(fresh_snapshot.smart_views.active_view_id, None);
}

#[tokio::test]
async fn agents_view_preferences_smart_views_set_active_copies_query_in_single_snapshot() {
    let mut fixture = Fixture::new().await;
    let query_notify = set_active_preferences_query(
        &mut fixture.client,
        saved_view_filters(),
        AgentSortMode::Status,
        AgentGroupMode::Project,
        true,
    )
    .await;
    let saved_query = query_notify.snapshot.preferences;

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Review Queue".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "save current notify").await;
    let view_id = SmartViewId::User(user_view_id(&notify.snapshot.smart_views.user[0]));

    send_set_agents_view_preferences(&mut fixture.client, AgentsViewPreferencesUpdate::Reset).await;
    expect_preferences_notify(&mut fixture.client, "reset before set active").await;
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetDensity {
            density: AgentListDensity::Compact,
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "set density before set active").await;
    let manual_order = vec![AgentOrderKey::Session {
        session_id: SessionId("manual-session".to_owned()),
    }];
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetManualOrder {
            manual_order: manual_order.clone(),
        },
    )
    .await;
    expect_preferences_notify(&mut fixture.client, "set manual order before set active").await;

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SetActive {
            id: view_id.clone(),
        },
    )
    .await;

    let notify = expect_preferences_notify(&mut fixture.client, "set active notify").await;
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(view_id.clone())
    );
    assert_eq!(notify.snapshot.preferences.filters, saved_query.filters);
    assert_eq!(notify.snapshot.preferences.sort_mode, saved_query.sort_mode);
    assert_eq!(
        notify.snapshot.preferences.group_mode,
        saved_query.group_mode
    );
    assert_eq!(
        notify.snapshot.preferences.hide_finished,
        saved_query.hide_finished
    );
    assert_eq!(
        notify.snapshot.preferences.density,
        AgentListDensity::Compact
    );
    assert_eq!(notify.snapshot.preferences.manual_order, manual_order);

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_snapshot = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences");
    assert_eq!(fresh_snapshot.smart_views.active_view_id, Some(view_id));
    assert_eq!(
        fresh_snapshot.preferences.filters,
        notify.snapshot.preferences.filters
    );
}

#[tokio::test]
async fn agents_view_preferences_smart_views_query_change_clears_active_view_id() {
    let mut fixture = Fixture::new().await;
    set_active_preferences_query(
        &mut fixture.client,
        saved_view_filters(),
        AgentSortMode::Status,
        AgentGroupMode::Project,
        true,
    )
    .await;
    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Saved Query".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "save current notify").await;
    let view_id = SmartViewId::User(user_view_id(&notify.snapshot.smart_views.user[0]));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SetActive {
            id: view_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "set active notify").await;
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(view_id.clone())
    );

    let changed_filters = AgentsViewFilters {
        host_ids: vec![],
        project_ids: vec![],
        statuses: vec![AgentStatusFilter::Thinking],
        backends: vec![BackendKind::Claude],
        origins: vec![protocol::AgentOrigin::AgentControl],
        tags: vec![],
    };
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetFilters {
            filters: changed_filters.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "set filters clears active").await;
    assert_eq!(notify.snapshot.smart_views.active_view_id, None);
    assert_eq!(notify.snapshot.preferences.filters, changed_filters);
    assert_eq!(notify.snapshot.smart_views.user.len(), 1);
    assert_eq!(notify.snapshot.smart_views.user[0].id, view_id);

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let fresh_snapshot = fresh_bootstrap
        .agents_view_preferences
        .expect("fresh bootstrap preferences");
    assert_eq!(fresh_snapshot.smart_views.active_view_id, None);
    assert_eq!(fresh_snapshot.smart_views.user.len(), 1);
}

#[tokio::test]
async fn agents_view_preferences_smart_views_manage_lifecycle_and_reject_built_ins() {
    let mut fixture = Fixture::new().await;
    set_active_preferences_query(
        &mut fixture.client,
        saved_view_filters(),
        AgentSortMode::NameAsc,
        AgentGroupMode::Backend,
        false,
    )
    .await;
    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "First View".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "save first notify").await;
    let first_id = SmartViewId::User(user_view_id(&notify.snapshot.smart_views.user[0]));

    set_active_preferences_query(
        &mut fixture.client,
        AgentsViewFilters {
            host_ids: vec![],
            project_ids: vec![],
            statuses: vec![AgentStatusFilter::Thinking],
            backends: vec![BackendKind::Claude],
            origins: vec![protocol::AgentOrigin::AgentControl],
            tags: vec![],
        },
        AgentSortMode::NewestFirst,
        AgentGroupMode::Status,
        true,
    )
    .await;
    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Second View".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "save second notify").await;
    let second_id = SmartViewId::User(user_view_id(&notify.snapshot.smart_views.user[1]));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Rename {
            id: first_id.clone(),
            name: "Renamed First".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "rename notify").await;
    assert_eq!(notify.snapshot.smart_views.user[0].name, "Renamed First");

    let updated_query = set_active_preferences_query(
        &mut fixture.client,
        AgentsViewFilters {
            host_ids: vec![HostFilterId(LOCAL_HOST_ID.to_owned())],
            project_ids: vec![],
            statuses: vec![AgentStatusFilter::Compacting],
            backends: vec![BackendKind::Kiro],
            origins: vec![protocol::AgentOrigin::Workflow],
            tags: vec![],
        },
        AgentSortMode::OldestFirst,
        AgentGroupMode::Project,
        true,
    )
    .await
    .snapshot
    .preferences;
    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Update {
            id: first_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "update notify").await;
    let first = &notify.snapshot.smart_views.user[0];
    assert_eq!(first.filters, updated_query.filters);
    assert_eq!(first.sort_mode, updated_query.sort_mode);
    assert_eq!(first.group_mode, updated_query.group_mode);
    assert_eq!(first.hide_finished, updated_query.hide_finished);

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Reorder {
            user_ids: vec![second_id.clone(), first_id.clone()],
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "reorder notify").await;
    assert_eq!(
        notify
            .snapshot
            .smart_views
            .user
            .iter()
            .map(|view| view.id.clone())
            .collect::<Vec<_>>(),
        vec![second_id.clone(), first_id.clone()]
    );

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Delete {
            id: second_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "delete notify").await;
    assert_eq!(notify.snapshot.smart_views.user.len(), 1);
    assert_eq!(notify.snapshot.smart_views.user[0].id, first_id.clone());
    assert_built_in_smart_views(&notify.snapshot.smart_views);

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SetActive {
            id: first_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "set first active notify").await;
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(first_id.clone())
    );
    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Delete {
            id: first_id.clone(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "delete active notify").await;
    assert!(notify.snapshot.smart_views.user.is_empty());
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(built_in_id(BuiltInSmartViewId::All))
    );
    assert_eq!(
        notify.snapshot.preferences.filters,
        AgentsViewFilters::default()
    );
    assert!(!notify.snapshot.preferences.hide_finished);

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Rename {
            id: built_in_id(BuiltInSmartViewId::All),
            name: "Nope".to_owned(),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "built-in rename error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert!(error.message.contains("built-in smart views"));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Update {
            id: built_in_id(BuiltInSmartViewId::Active),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "built-in update error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert!(error.message.contains("built-in smart views"));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Delete {
            id: built_in_id(BuiltInSmartViewId::FailedTerminated),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "built-in delete error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert!(error.message.contains("built-in smart views"));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::Reorder {
            user_ids: vec![built_in_id(BuiltInSmartViewId::All)],
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "built-in reorder error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert!(error.message.contains("built-in smart views"));

    send_set_agents_smart_views(
        &mut fixture.client,
        AgentsSmartViewsUpdate::SetActive {
            id: user_id("missing-view"),
        },
    )
    .await;
    let error = expect_command_error(&mut fixture.client, "unknown set active error").await;
    assert_eq!(error.request_kind, FrameKind::SetAgentsSmartViews);
    assert!(error.message.contains("unknown smart view id"));
}

#[tokio::test]
async fn agents_view_preferences_smart_views_migrates_legacy_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let preferences_path = dir.path().join("agents_view_preferences.json");
    let legacy = serde_json::json!({
        "version": 1,
        "preferences": {
            "filters": {
                "host_ids": [LOCAL_HOST_ID],
                "project_ids": [],
                "statuses": ["idle"],
                "backends": ["codex"],
                "origins": ["user"]
            },
            "sort_mode": "name_asc",
            "group_mode": "status",
            "density": "compact",
            "hide_finished": true,
            "manual_order": []
        }
    });
    std::fs::write(
        &preferences_path,
        serde_json::to_string_pretty(&legacy).expect("serialize legacy store"),
    )
    .expect("write legacy preferences store");
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");

    let (mut client, bootstrap) = connect_host(host).await;
    let snapshot = bootstrap
        .agents_view_preferences
        .expect("primary bootstrap preferences");
    assert_eq!(snapshot.preferences.sort_mode, AgentSortMode::NameAsc);
    assert_eq!(snapshot.preferences.group_mode, AgentGroupMode::Status);
    assert_eq!(snapshot.preferences.density, AgentListDensity::Compact);
    assert!(snapshot.preferences.hide_finished);
    assert_eq!(snapshot.sidebar, AgentsSidebarPreferences::default());
    assert_built_in_smart_views(&snapshot.smart_views);
    assert!(snapshot.smart_views.user.is_empty());
    assert_eq!(
        snapshot.smart_views.active_view_id,
        Some(built_in_id(BuiltInSmartViewId::All))
    );

    send_set_agents_smart_views(
        &mut client,
        AgentsSmartViewsUpdate::SaveCurrent {
            name: "Migrated View".to_owned(),
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut client, "legacy save current notify").await;
    assert_eq!(notify.snapshot.smart_views.user.len(), 1);
    assert_eq!(
        notify.snapshot.smart_views.user[0].id,
        user_id("migrated-view")
    );
    assert!(
        std::fs::read_to_string(preferences_path)
            .expect("read migrated preferences store")
            .contains("\"version\": 5")
    );
}

#[tokio::test]
async fn agents_view_preferences_smart_views_v2_store_migrates_to_v3_with_user_views() {
    let dir = tempfile::tempdir().expect("tempdir");
    let preferences_path = dir.path().join("agents_view_preferences.json");
    let v2_store = serde_json::json!({
        "version": 2,
        "preferences": {
            "filters": {
                "host_ids": [],
                "project_ids": [],
                "statuses": [],
                "backends": [],
                "origins": []
            },
            "sort_mode": "manual_then_activity",
            "group_mode": "flat",
            "density": "comfortable",
            "hide_finished": false,
            "manual_order": []
        },
        "smart_views": {
            "user": [{
                "id": { "kind": "user", "id": "review-queue" },
                "name": "Review Queue",
                "filters": {
                    "host_ids": [LOCAL_HOST_ID],
                    "project_ids": [],
                    "statuses": ["idle"],
                    "backends": ["codex"],
                    "origins": ["user"]
                },
                "sort_mode": "name_asc",
                "group_mode": "backend",
                "hide_finished": true
            }],
            "active_view_id": { "kind": "user", "id": "review-queue" }
        }
    });
    std::fs::write(
        &preferences_path,
        serde_json::to_string_pretty(&v2_store).expect("serialize v2 store"),
    )
    .expect("write v2 preferences store");
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");

    let (mut client, bootstrap) = connect_host(host).await;
    let snapshot = bootstrap
        .agents_view_preferences
        .expect("primary bootstrap preferences");
    assert_built_in_smart_views(&snapshot.smart_views);
    assert_eq!(
        snapshot.smart_views.active_view_id,
        Some(user_id("review-queue"))
    );
    assert_eq!(snapshot.smart_views.user.len(), 1);
    let view = &snapshot.smart_views.user[0];
    assert_eq!(view.id, user_id("review-queue"));
    assert_eq!(view.name, "Review Queue");
    assert_eq!(
        view.filters.host_ids,
        vec![HostFilterId(LOCAL_HOST_ID.to_owned())]
    );
    assert_eq!(view.filters.statuses, vec![AgentStatusFilter::Idle]);
    assert_eq!(view.filters.backends, vec![BackendKind::Codex]);
    assert_eq!(view.filters.origins, vec![protocol::AgentOrigin::User]);
    assert!(view.filters.tags.is_empty());
    assert_eq!(view.sort_mode, AgentSortMode::NameAsc);
    assert_eq!(view.group_mode, AgentGroupMode::Backend);
    assert!(view.hide_finished);
    assert_eq!(snapshot.sidebar, AgentsSidebarPreferences::default());
    assert!(snapshot.tags.manual.is_empty());
    assert!(snapshot.tags.manual_assignments.is_empty());
    assert!(snapshot.tags.system.is_empty());
    assert!(snapshot.tags.system_assignments.is_empty());
    assert!(snapshot.pins.pinned.is_empty());

    send_set_agents_view_preferences(
        &mut client,
        AgentsViewPreferencesUpdate::SetDensity {
            density: AgentListDensity::Compact,
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut client, "v2 migration rewrite notify").await;
    assert_eq!(notify.snapshot.smart_views.user, snapshot.smart_views.user);
    assert_eq!(
        notify.snapshot.smart_views.active_view_id,
        Some(user_id("review-queue"))
    );
    assert!(notify.snapshot.tags.manual.is_empty());
    assert!(notify.snapshot.tags.manual_assignments.is_empty());
    assert!(notify.snapshot.pins.pinned.is_empty());
    assert!(
        std::fs::read_to_string(preferences_path)
            .expect("read migrated preferences store")
            .contains("\"version\": 5")
    );
}

#[tokio::test]
async fn agents_view_preferences_host_filter_survives_new_host_stream_path() {
    let mut fixture = Fixture::new().await;
    let first_host_stream = single_host_stream(&fixture.client);

    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetFilters {
            filters: AgentsViewFilters {
                host_ids: vec![HostFilterId(LOCAL_HOST_ID.to_owned())],
                project_ids: vec![],
                statuses: vec![],
                backends: vec![],
                origins: vec![],
                tags: vec![],
            },
        },
    )
    .await;
    let notify = expect_preferences_notify(&mut fixture.client, "host filter notify").await;
    assert_eq!(
        notify.snapshot.preferences.filters.host_ids,
        vec![HostFilterId(LOCAL_HOST_ID.to_owned())]
    );

    let (new_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_host_stream = single_host_stream(&new_client);
    assert_ne!(
        first_host_stream, second_host_stream,
        "reconnect should use a new /host/<uuid> stream path"
    );
    assert_eq!(
        bootstrap
            .agents_view_preferences
            .expect("reconnect preferences")
            .preferences
            .filters
            .host_ids,
        vec![HostFilterId(LOCAL_HOST_ID.to_owned())]
    );
}

#[tokio::test]
async fn agents_view_preferences_manual_order_drops_non_local_transient_keys() {
    let mut fixture = Fixture::new().await;
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetManualOrder {
            manual_order: vec![AgentOrderKey::TransientAgent {
                host_id: HostFilterId("remote-host".to_owned()),
                agent_id: AgentId("remote-agent".to_owned()),
            }],
        },
    )
    .await;

    let notify =
        expect_preferences_notify(&mut fixture.client, "remote transient order notify").await;
    assert!(notify.snapshot.preferences.manual_order.is_empty());
}

#[tokio::test]
async fn agents_view_preferences_manual_order_is_canonicalized() {
    let mut fixture = Fixture::new().await;
    let agent = spawn_agent_for_order(&mut fixture.client).await;
    let start = expect_agent_start_payload(&mut fixture.client, &agent.instance_stream).await;
    let known_session = start
        .session_id
        .expect("mock backend should assign a session id");

    let pinned = SessionId("pinned-session".to_owned());
    let offline = SessionId("offline-session".to_owned());
    send_set_agents_view_preferences(
        &mut fixture.client,
        AgentsViewPreferencesUpdate::SetManualOrder {
            manual_order: vec![
                AgentOrderKey::Session {
                    session_id: pinned.clone(),
                },
                AgentOrderKey::Session {
                    session_id: pinned.clone(),
                },
                AgentOrderKey::TransientAgent {
                    host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
                    agent_id: AgentId("unknown-agent".to_owned()),
                },
                AgentOrderKey::TransientAgent {
                    host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
                    agent_id: agent.agent_id,
                },
                AgentOrderKey::Session {
                    session_id: offline.clone(),
                },
                AgentOrderKey::Session {
                    session_id: known_session.clone(),
                },
            ],
        },
    )
    .await;

    let notify =
        expect_preferences_notify_where(&mut fixture.client, "manual order notify", |notify| {
            !notify.snapshot.preferences.manual_order.is_empty()
        })
        .await;
    assert_eq!(
        notify.snapshot.preferences.manual_order,
        vec![
            AgentOrderKey::Session { session_id: pinned },
            AgentOrderKey::Session {
                session_id: known_session,
            },
            AgentOrderKey::Session {
                session_id: offline,
            },
        ]
    );
}
