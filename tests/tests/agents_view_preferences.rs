mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentGroupMode, AgentId, AgentListDensity,
    AgentOrderKey, AgentSortMode, AgentStartPayload, AgentStatusFilter, AgentsSmartViewsSnapshot,
    AgentsSmartViewsUpdate, AgentsViewFilters, AgentsViewPreferences,
    AgentsViewPreferencesNotifyPayload, AgentsViewPreferencesStoreErrorKind,
    AgentsViewPreferencesUpdate, BackendKind, BuiltInSmartViewId, CommandErrorCode,
    CommandErrorPayload, Envelope, FrameKind, HostBootstrapPayload, HostFilterId, LOCAL_HOST_ID,
    NewAgentPayload, SessionId, SetAgentsSmartViewsPayload, SetAgentsViewPreferencesPayload,
    SmartView, SmartViewId, SpawnAgentParams, SpawnAgentPayload, StreamPath, UserSmartViewId,
    read_envelope, write_envelope,
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

async fn expect_client_kind_any_agent_start(
    client: &mut client::Connection,
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
        if matches!(env.kind, FrameKind::AgentStart | FrameKind::AgentBootstrap) {
            return env;
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

    let filters = AgentsViewFilters {
        host_ids: vec![HostFilterId(LOCAL_HOST_ID.to_owned())],
        project_ids: vec![],
        statuses: vec![protocol::AgentStatusFilter::Idle],
        backends: vec![BackendKind::Codex, BackendKind::Claude],
        origins: vec![protocol::AgentOrigin::Workflow, protocol::AgentOrigin::User],
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
            .contains("\"version\": 2")
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

    let notify = expect_preferences_notify(&mut fixture.client, "manual order notify").await;
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
