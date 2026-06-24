mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentId, AgentOrderKey, AgentStartPayload,
    AgentsViewFilters, AgentsViewPreferences, AgentsViewPreferencesNotifyPayload,
    AgentsViewPreferencesStoreErrorKind, AgentsViewPreferencesUpdate, BackendKind,
    CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, HostBootstrapPayload, HostFilterId,
    LOCAL_HOST_ID, NewAgentPayload, SessionId, SetAgentsViewPreferencesPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath, read_envelope, write_envelope,
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
}

#[tokio::test]
async fn agents_view_preferences_update_notifies_and_persists_to_bootstrap() {
    let mut fixture = Fixture::new().await;
    assert_eq!(
        fixture.bootstrap.agents_view_preferences,
        Some(protocol::AgentsViewPreferencesSnapshot {
            preferences: AgentsViewPreferences::default(),
            load_error: None,
        })
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

    send_set_agents_view_preferences(&mut client, AgentsViewPreferencesUpdate::Reset).await;
    let notify = expect_preferences_notify(&mut client, "reset preferences notify").await;
    assert_eq!(
        notify.snapshot.preferences,
        AgentsViewPreferences::default()
    );
    assert!(notify.snapshot.load_error.is_none());
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
