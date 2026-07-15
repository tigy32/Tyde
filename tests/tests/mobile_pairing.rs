use std::time::Duration;

mod support;

use mqtt_transport::{
    MobilePairingQrPayload, MqttConnectConfig, MqttTransportPolicy, ParticipantRole,
    host_to_client_topic,
};
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, BackendKind, BrokerUrl, ChatEvent,
    CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, HostBootstrapPayload,
    HostSettingValue, ListSessionsPayload, LoadAgentPayload, MobileAccessErrorCode,
    MobileAccessStatePayload, MobileBrokerStatus, MobileDeviceState, MobilePairingOfferPayload,
    MobilePairingStartPayload, MobilePairingState, NewAgentPayload, ProjectCreatePayload,
    ProjectRootPath, SendMessagePayload, SessionId, SessionListPageStatus, SetSettingPayload,
    SpawnAgentParams, SpawnAgentPayload, StreamPath, TerminalCreatePayload, TerminalLaunchTarget,
    write_envelope,
};
use server::backend::BackendSession;
use server::store::session::SessionStore;
use tokio::time::timeout;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

struct Harness {
    host: server::HostHandle,
    _store_dir: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        Self::with_seeded_sessions(0).await
    }

    async fn with_seeded_sessions(session_count: u32) -> Self {
        let store_dir = tempfile::tempdir().expect("create mobile pairing test store dir");
        let session_path = store_dir.path().join("sessions.json");
        seed_session_store(&session_path, session_count);
        let host = server::spawn_host_with_mock_backend(
            session_path,
            store_dir.path().join("projects.json"),
            store_dir.path().join("settings.json"),
        )
        .expect("spawn test host");
        Self {
            host,
            _store_dir: store_dir,
        }
    }

    async fn connect_desktop(&self) -> client::Connection {
        connect_desktop(self.host.clone()).await
    }
}

fn seed_session_store(path: &std::path::Path, count: u32) {
    let store = SessionStore::load(path.to_owned()).expect("load session store");
    for index in 0..count {
        store
            .upsert_backend_session(
                &BackendSession {
                    id: SessionId(format!("session-{index:04}")),
                    backend_kind: BackendKind::Claude,
                    workspace_roots: vec![format!("/workspace/{index}")],
                    title: Some(format!("Session {index:04}")),
                    token_count: Some(index as u64),
                    created_at_ms: Some(index as u64),
                    updated_at_ms: Some((count - index) as u64),
                    resumable: true,
                },
                None,
                None,
                None,
                None,
            )
            .expect("seed backend session");
    }
}

async fn connect_desktop(host: server::HostHandle) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig::current();

    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake failed");
        if let Err(error) = server::run_connection(conn, host).await {
            eprintln!("server connection loop failed: {error:?}");
        }
    });

    client::connect(&client_config, client_stream)
        .await
        .expect("client handshake failed")
}

async fn next_event(client: &mut client::Connection, context: &str) -> Envelope {
    timeout(EVENT_TIMEOUT, client.next_event())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
        .unwrap_or_else(|error| panic!("failed reading {context}: {error:?}"))
        .unwrap_or_else(|| panic!("connection closed while waiting for {context}"))
}

async fn expect_next_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    let env = next_event(client, context).await;
    assert_eq!(
        env.kind, kind,
        "unexpected frame while waiting for {context}"
    );
    env
}

async fn wait_for_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    loop {
        let env = next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while waiting for {context}: {payload:?}");
        }
        if env.kind == kind {
            return env;
        }
    }
}

async fn wait_for_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    loop {
        let env = next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            return env.parse_payload().expect("parse CommandError");
        }
    }
}

async fn wait_for_chat_stream_end(client: &mut client::Connection, context: &str) -> ChatEvent {
    loop {
        let env = next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while waiting for {context}: {payload:?}");
        }
        if env.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            return event;
        }
    }
}

async fn expect_initial_replay(client: &mut client::Connection) -> MobileAccessStatePayload {
    let env = expect_next_kind(client, FrameKind::HostBootstrap, "initial HostBootstrap").await;
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse HostBootstrap");
    let state = bootstrap.mobile_access;
    assert_eq!(state.broker_status, MobileBrokerStatus::Disabled);
    assert_eq!(state.pairing, MobilePairingState::Idle);
    state
}

async fn set_mobile_broker_url(client: &mut client::Connection, broker_url: Option<BrokerUrl>) {
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::MobileBrokerUrl { broker_url },
        })
        .await
        .expect("set mobile broker URL");
}

async fn set_mobile_enabled(client: &mut client::Connection, enabled: bool) {
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnableMobileConnections { enabled },
        })
        .await
        .expect("set enable_mobile_connections");
}

async fn wait_for_mobile_state(
    client: &mut client::Connection,
    predicate: impl Fn(&MobileAccessStatePayload) -> bool,
    context: &str,
) -> MobileAccessStatePayload {
    loop {
        let env = next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while waiting for {context}: {payload:?}");
        }
        if env.kind != FrameKind::MobileAccessState {
            continue;
        }
        let state: MobileAccessStatePayload = env.parse_payload().expect("parse MobileAccessState");
        if predicate(&state) {
            return state;
        }
    }
}

async fn send_mobile_pairing_start(client: &mut client::Connection) {
    send_host_payload(
        client,
        FrameKind::MobilePairingStart,
        &MobilePairingStartPayload {},
    )
    .await;
}

async fn send_host_payload<T: serde::Serialize>(
    client: &mut client::Connection,
    kind: FrameKind,
    payload: &T,
) {
    let stream = host_stream(client);
    send_stream_payload(client, stream, kind, payload).await;
}

async fn send_stream_payload<T: serde::Serialize>(
    client: &mut client::Connection,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) {
    let seq = client
        .outgoing_seq
        .get(&stream)
        .copied()
        .expect("missing host stream sequence counter");
    let envelope =
        Envelope::from_payload(stream.clone(), kind, seq, payload).expect("serialize host payload");
    client.outgoing_seq.insert(stream, seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write payload");
}

fn host_stream(client: &client::Connection) -> StreamPath {
    let mut host_streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let stream = host_streams.next().cloned().expect("missing host stream");
    assert!(
        host_streams.next().is_none(),
        "expected exactly one host stream"
    );
    stream
}

async fn load_mobile_agent(client: &mut client::Connection, agent: &NewAgentPayload) {
    send_stream_payload(
        client,
        agent.instance_stream.clone(),
        FrameKind::LoadAgent,
        &LoadAgentPayload {},
    )
    .await;
    let _ = wait_for_kind(client, FrameKind::AgentBootstrap, "mobile AgentBootstrap").await;
}

async fn wait_for_agent_bootstrap_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentBootstrapPayload {
    loop {
        let env = next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while waiting for {context}: {payload:?}");
        }
        if env.kind != FrameKind::AgentBootstrap {
            continue;
        }
        assert_eq!(
            &env.stream, stream,
            "AgentBootstrap arrived on the wrong instance stream"
        );
        return env.parse_payload().expect("parse AgentBootstrap");
    }
}

fn assert_initial_mock_response(bootstrap: &AgentBootstrapPayload, prompt: &str, context: &str) {
    let expected = format!("mock backend response to: {prompt}");
    let responses = bootstrap
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
        .filter(|content| content.contains(expected.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        responses.len(),
        1,
        "{context} should contain exactly one authoritative initial mock response: {:?}",
        bootstrap.events
    );
}

#[tokio::test]
async fn mqtt_mobile_new_chat_spawns_and_loads_agent() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");
    let mut mobile = connect_mobile_client(&qr).await;
    let bootstrap_env = expect_next_kind(
        &mut mobile,
        FrameKind::HostBootstrap,
        "empty mobile HostBootstrap",
    )
    .await;
    let bootstrap: HostBootstrapPayload = bootstrap_env
        .parse_payload()
        .expect("parse empty mobile HostBootstrap");
    assert!(bootstrap.agents.is_empty());

    let prompt = "initial prompt from mobile new chat";
    let mobile_host_stream = host_stream(&mobile);
    send_host_payload(
        &mut mobile,
        FrameKind::SpawnAgent,
        &SpawnAgentPayload {
            name: Some("mobile new chat agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        },
    )
    .await;

    let mobile_new_env = wait_for_kind(&mut mobile, FrameKind::NewAgent, "mobile NewAgent").await;
    assert_eq!(mobile_new_env.stream, mobile_host_stream);
    let mobile_agent: NewAgentPayload = mobile_new_env.parse_payload().expect("parse NewAgent");

    let desktop_new_env =
        wait_for_kind(&mut desktop, FrameKind::NewAgent, "desktop NewAgent").await;
    let desktop_agent: NewAgentPayload = desktop_new_env
        .parse_payload()
        .expect("parse desktop NewAgent");
    assert_eq!(desktop_agent.agent_id, mobile_agent.agent_id);
    assert_ne!(desktop_agent.instance_stream, mobile_agent.instance_stream);
    let desktop_end = wait_for_chat_stream_end(&mut desktop, "desktop initial StreamEnd").await;
    let ChatEvent::StreamEnd(desktop_end) = desktop_end else {
        panic!("expected desktop StreamEnd");
    };
    assert!(
        desktop_end
            .message
            .content
            .contains(&format!("mock backend response to: {prompt}")),
        "unexpected desktop response: {}",
        desktop_end.message.content
    );

    send_stream_payload(
        &mut mobile,
        mobile_agent.instance_stream.clone(),
        FrameKind::LoadAgent,
        &LoadAgentPayload {},
    )
    .await;
    let first_bootstrap = wait_for_agent_bootstrap_on_stream(
        &mut mobile,
        &mobile_agent.instance_stream,
        "mobile-created AgentBootstrap",
    )
    .await;
    assert_initial_mock_response(&first_bootstrap, prompt, "first mobile load");

    let mut reconnected = connect_mobile_client(&qr).await;
    let reconnected_agent =
        expect_mobile_replay(&mut reconnected, 0, "reconnected mobile replay").await;
    assert_eq!(reconnected_agent.agent_id, mobile_agent.agent_id);
    assert_ne!(
        reconnected_agent.instance_stream,
        mobile_agent.instance_stream
    );
    send_stream_payload(
        &mut reconnected,
        reconnected_agent.instance_stream.clone(),
        FrameKind::LoadAgent,
        &LoadAgentPayload {},
    )
    .await;
    let replayed_bootstrap = wait_for_agent_bootstrap_on_stream(
        &mut reconnected,
        &reconnected_agent.instance_stream,
        "reconnected mobile AgentBootstrap",
    )
    .await;
    assert_initial_mock_response(&replayed_bootstrap, prompt, "reconnected mobile load");
}

#[tokio::test]
async fn mqtt_mobile_duplicate_load_agent_reports_command_error() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    desktop
        .spawn_agent(SpawnAgentPayload {
            name: Some("mobile duplicate load agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "initial mobile duplicate load prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn duplicate load agent");
    let _ = wait_for_kind(&mut desktop, FrameKind::NewAgent, "desktop NewAgent").await;
    let _ = wait_for_chat_stream_end(&mut desktop, "desktop initial StreamEnd").await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");
    let mut mobile = connect_mobile_client(&qr).await;
    let replayed_agent = expect_mobile_replay(&mut mobile, 0, "mobile duplicate load replay").await;

    load_mobile_agent(&mut mobile, &replayed_agent).await;
    send_stream_payload(
        &mut mobile,
        replayed_agent.instance_stream.clone(),
        FrameKind::LoadAgent,
        &LoadAgentPayload {},
    )
    .await;

    let error = loop {
        let env = next_event(&mut mobile, "duplicate mobile LoadAgent").await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == replayed_agent.instance_stream {
            panic!("rejected duplicate LoadAgent produced a false AgentBootstrap success");
        }
        if env.kind == FrameKind::CommandError {
            break env
                .parse_payload::<CommandErrorPayload>()
                .expect("parse duplicate LoadAgent CommandError");
        }
    };
    assert_eq!(error.stream, replayed_agent.instance_stream);
    assert_eq!(error.request_kind, FrameKind::LoadAgent);
    assert_eq!(error.operation, "load_agent");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("already attached"),
        "unexpected duplicate LoadAgent message: {}",
        error.message
    );

    send_host_payload(
        &mut mobile,
        FrameKind::ListSessions,
        &ListSessionsPayload::default(),
    )
    .await;
    loop {
        let env = next_event(&mut mobile, "SessionList after rejected LoadAgent").await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == replayed_agent.instance_stream {
            panic!("rejected duplicate LoadAgent later produced a false AgentBootstrap success");
        }
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while checking rejected LoadAgent: {payload:?}");
        }
        if env.kind == FrameKind::SessionList {
            let sessions: protocol::SessionListPayload =
                env.parse_payload().expect("parse SessionList");
            assert_eq!(sessions.sessions.len(), 1);
            break;
        }
    }
}

#[tokio::test]
async fn enabling_mobile_without_pairing_fails_closed() {
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    set_mobile_enabled(&mut desktop, true).await;
    let repair = wait_for_mobile_state(
        &mut desktop,
        |state| {
            matches!(
                state.broker_status,
                MobileBrokerStatus::RepairRequired {
                    code: MobileAccessErrorCode::RepairRequired,
                    ..
                }
            )
        },
        "MobileBrokerStatus::RepairRequired",
    )
    .await;
    match repair.broker_status {
        MobileBrokerStatus::RepairRequired { message, .. } => {
            assert!(message.contains("tycode.dev"));
        }
        other => panic!("expected RepairRequired broker status, got {other:?}"),
    }
}

#[tokio::test]
async fn plaintext_public_mqtt_url_is_rejected_at_settings_write() {
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    desktop
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::MobileBrokerUrl {
                broker_url: Some(
                    BrokerUrl::new("mqtt://broker.example.test:1883").expect("broker URL"),
                ),
            },
        })
        .await
        .expect("send invalid broker setting");
    let error = wait_for_command_error(&mut desktop, "invalid mobile broker setting").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(error.message.contains("insecure"));
}

#[tokio::test]
async fn pairing_qr_embeds_configured_mqtt_endpoint_and_secret_room() {
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;
    let broker_url = BrokerUrl::new("mqtts://127.0.0.1:8883").expect("broker URL");

    set_mobile_broker_url(&mut desktop, Some(broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");

    assert_eq!(qr.broker.url, broker_url);
    assert_eq!(qr.policy, MqttTransportPolicy::default());
    assert_eq!(
        host_to_client_topic(&qr.room),
        format!("tyde/v1/{}/host-to-client", qr.room)
    );
    assert_eq!(
        mqtt_transport::client_to_host_topic(&qr.room),
        format!("tyde/v1/{}/client-to-host", qr.room)
    );
    assert_eq!(qr.psk.as_bytes().len(), 32);
}

#[tokio::test]
async fn mqtt_pairing_accepts_mobile_tyde_hello_over_encrypted_stream() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let project_root = tempfile::tempdir().expect("create mobile project root");
    desktop
        .project_create(ProjectCreatePayload {
            name: "Mobile Project".to_owned(),
            roots: vec![ProjectRootPath(
                project_root.path().to_string_lossy().into_owned(),
            )],
        })
        .await
        .expect("create project for mobile replay");
    wait_for_kind(
        &mut desktop,
        FrameKind::ProjectNotify,
        "desktop ProjectNotify",
    )
    .await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");
    assert_eq!(qr.broker.url, broker.broker_url);

    let mobile_stream = timeout(
        EVENT_TIMEOUT,
        mqtt_transport::connect_ephemeral(MqttConnectConfig {
            endpoint: qr.broker.clone(),
            room: qr.room,
            psk: qr.psk.clone(),
            role: ParticipantRole::Client,
        }),
    )
    .await
    .expect("timed out connecting mobile MQTT transport")
    .expect("mobile MQTT transport");
    let mut mobile = timeout(
        EVENT_TIMEOUT,
        client::connect(&client::ClientConfig::current(), mobile_stream),
    )
    .await
    .expect("timed out waiting for mobile Tyde Hello")
    .expect("mobile Tyde Hello");

    let state = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.pairing, MobilePairingState::Consumed { .. }),
        "consumed mobile pairing",
    )
    .await;
    assert!(matches!(
        state.paired_devices.first().map(|device| &device.state),
        Some(MobileDeviceState::Connected)
    ));

    let env = expect_next_kind(
        &mut mobile,
        FrameKind::HostBootstrap,
        "mobile HostBootstrap",
    )
    .await;
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse mobile HostBootstrap");
    assert_eq!(bootstrap.projects.len(), 1);
    let _ = wait_for_kind(
        &mut mobile,
        FrameKind::ProjectBootstrap,
        "mobile ProjectBootstrap",
    )
    .await;

    send_host_payload(
        &mut mobile,
        FrameKind::ListSessions,
        &ListSessionsPayload::default(),
    )
    .await;
    let _ = wait_for_kind(&mut mobile, FrameKind::SessionList, "mobile SessionList").await;

    send_host_payload(
        &mut mobile,
        FrameKind::TerminalCreate,
        &TerminalCreatePayload {
            target: TerminalLaunchTarget::HostDefault,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let error = wait_for_command_error(&mut mobile, "mobile terminal command rejection").await;
    assert_eq!(error.request_kind, FrameKind::TerminalCreate);
    assert!(
        error.message.contains("not allowed from Tyde Mobile"),
        "unexpected terminal rejection: {error:?}"
    );
}

#[tokio::test]
async fn mqtt_mobile_bootstrap_pages_large_session_store() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::with_seeded_sessions(300).await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");
    let mut mobile = connect_mobile_client(&qr).await;

    let env = expect_next_kind(
        &mut mobile,
        FrameKind::HostBootstrap,
        "large mobile HostBootstrap",
    )
    .await;
    let serialized_len = serde_json::to_vec(&env)
        .expect("serialize mobile HostBootstrap envelope")
        .len();
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse HostBootstrap");
    assert_eq!(
        bootstrap.session_list.scope,
        protocol::SessionListScope::RootSessions
    );
    assert_eq!(bootstrap.session_list.total_count, 300);
    assert_eq!(
        bootstrap.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );
    assert!(
        serialized_len < 128 * 1024,
        "mobile HostBootstrap over MQTT should stay bounded, got {serialized_len} bytes"
    );

    let mut loaded = bootstrap.sessions.len();
    let mut next_cursor = match bootstrap.session_list.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("large mobile bootstrap should be paged"),
    };
    loop {
        send_host_payload(
            &mut mobile,
            FrameKind::ListSessions,
            &ListSessionsPayload {
                scope: Some(protocol::SessionListScope::RootSessions),
                cursor: Some(next_cursor),
                limit: Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT),
            },
        )
        .await;
        let env = wait_for_kind(
            &mut mobile,
            FrameKind::SessionList,
            "large mobile SessionList page",
        )
        .await;
        let page: protocol::SessionListPayload = env.parse_payload().expect("parse SessionList");
        assert_eq!(page.page.scope, protocol::SessionListScope::RootSessions);
        assert_eq!(page.page.total_count, 300);
        loaded += page.sessions.len();
        match page.page.status {
            SessionListPageStatus::More { next_cursor: next } => next_cursor = next,
            SessionListPageStatus::Complete => break,
        }
    }

    assert_eq!(loaded, 300);
}

#[tokio::test]
async fn mqtt_mobile_receives_agent_replay_sessions_and_chat_events() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let mut project_roots = Vec::new();
    for index in 0..12 {
        let project_root = tempfile::tempdir().expect("create mobile project root");
        std::fs::write(
            project_root.path().join(format!("file-{index}.txt")),
            format!("mobile project file {index}"),
        )
        .expect("write project file");
        desktop
            .project_create(ProjectCreatePayload {
                name: format!("Mobile Project {index}"),
                roots: vec![ProjectRootPath(
                    project_root.path().to_string_lossy().into_owned(),
                )],
            })
            .await
            .expect("create project for mobile replay");
        wait_for_kind(
            &mut desktop,
            FrameKind::ProjectNotify,
            "desktop ProjectNotify",
        )
        .await;
        project_roots.push(project_root);
    }

    desktop
        .spawn_agent(SpawnAgentPayload {
            name: Some("mobile replay agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![project_roots[0].path().to_string_lossy().into_owned()],
                prompt: "initial mobile replay prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn replay agent");
    let _ = wait_for_kind(&mut desktop, FrameKind::NewAgent, "desktop NewAgent").await;
    let _ = wait_for_chat_stream_end(&mut desktop, "desktop initial StreamEnd").await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");

    let mobile_stream = timeout(
        EVENT_TIMEOUT,
        mqtt_transport::connect_ephemeral(MqttConnectConfig {
            endpoint: qr.broker.clone(),
            room: qr.room,
            psk: qr.psk.clone(),
            role: ParticipantRole::Client,
        }),
    )
    .await
    .expect("timed out connecting mobile MQTT transport")
    .expect("mobile MQTT transport");
    let mut mobile = timeout(
        EVENT_TIMEOUT,
        client::connect(&client::ClientConfig::current(), mobile_stream),
    )
    .await
    .expect("timed out waiting for mobile Tyde Hello")
    .expect("mobile Tyde Hello");

    let mut project_count = 0;
    let mut replayed_agent = None;
    while replayed_agent.is_none() || project_count < 12 {
        let env = next_event(&mut mobile, "mobile initial replay").await;
        match env.kind {
            FrameKind::HostBootstrap => {
                let bootstrap: HostBootstrapPayload =
                    env.parse_payload().expect("parse HostBootstrap");
                project_count = bootstrap.projects.len();
                replayed_agent = bootstrap.agents.into_iter().next();
            }
            FrameKind::CommandError => {
                let payload: CommandErrorPayload =
                    env.parse_payload().expect("parse command error");
                panic!("command error during mobile replay: {payload:?}");
            }
            _ => {}
        }
    }
    assert_eq!(project_count, 12);
    let replayed_agent = replayed_agent.expect("replayed agent");

    send_host_payload(
        &mut mobile,
        FrameKind::ListSessions,
        &ListSessionsPayload::default(),
    )
    .await;
    let session_env =
        wait_for_kind(&mut mobile, FrameKind::SessionList, "mobile SessionList").await;
    let sessions: protocol::SessionListPayload =
        session_env.parse_payload().expect("parse SessionList");
    assert_eq!(sessions.sessions.len(), 1);

    load_mobile_agent(&mut mobile, &replayed_agent).await;

    mobile
        .send_message_payload(
            &replayed_agent.instance_stream,
            SendMessagePayload {
                message: "hello from mobile mqtt test".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("send mobile message");
    let event = wait_for_chat_stream_end(&mut mobile, "mobile follow-up StreamEnd").await;
    let ChatEvent::StreamEnd(end) = event else {
        panic!("expected StreamEnd");
    };
    assert!(
        end.message
            .content
            .contains("mock backend response to: hello from mobile mqtt test"),
        "unexpected final message: {}",
        end.message.content
    );
}

#[tokio::test]
async fn mqtt_mobile_reconnect_replays_bootstrap_state_again() {
    let broker = support::start_plain_mqtt_broker().expect("start local MQTT broker");
    let harness = Harness::new().await;
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let project_root = tempfile::tempdir().expect("create mobile project root");
    std::fs::write(project_root.path().join("file.txt"), "mobile project file")
        .expect("write project file");
    desktop
        .project_create(ProjectCreatePayload {
            name: "Mobile Reconnect Project".to_owned(),
            roots: vec![ProjectRootPath(
                project_root.path().to_string_lossy().into_owned(),
            )],
        })
        .await
        .expect("create project for mobile replay");
    wait_for_kind(
        &mut desktop,
        FrameKind::ProjectNotify,
        "desktop ProjectNotify",
    )
    .await;

    desktop
        .spawn_agent(SpawnAgentPayload {
            name: Some("mobile reconnect agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![project_root.path().to_string_lossy().into_owned()],
                prompt: "initial mobile reconnect prompt".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn replay agent");
    let _ = wait_for_kind(&mut desktop, FrameKind::NewAgent, "desktop NewAgent").await;
    let _ = wait_for_chat_stream_end(&mut desktop, "desktop initial StreamEnd").await;

    set_mobile_broker_url(&mut desktop, Some(broker.broker_url.clone())).await;
    set_mobile_enabled(&mut desktop, true).await;
    let _ = wait_for_mobile_state(
        &mut desktop,
        |state| matches!(state.broker_status, MobileBrokerStatus::Online { .. }),
        "MobileBrokerStatus::Online",
    )
    .await;
    send_mobile_pairing_start(&mut desktop).await;

    let offer_env = wait_for_kind(
        &mut desktop,
        FrameKind::MobilePairingOffer,
        "MobilePairingOffer",
    )
    .await;
    let offer: MobilePairingOfferPayload = offer_env.parse_payload().expect("parse offer");
    let qr = MobilePairingQrPayload::from_any(&offer.qr_uri.0).expect("parse QR");

    let mut first = connect_mobile_client(&qr).await;
    expect_mobile_replay(&mut first, 1, "first mobile replay").await;

    let mut second = connect_mobile_client(&qr).await;
    let replayed_agent = expect_mobile_replay(&mut second, 1, "second mobile replay").await;

    send_host_payload(
        &mut second,
        FrameKind::ListSessions,
        &ListSessionsPayload::default(),
    )
    .await;
    let session_env = wait_for_kind(
        &mut second,
        FrameKind::SessionList,
        "second mobile SessionList",
    )
    .await;
    let sessions: protocol::SessionListPayload =
        session_env.parse_payload().expect("parse SessionList");
    assert_eq!(sessions.sessions.len(), 1);

    load_mobile_agent(&mut second, &replayed_agent).await;

    second
        .send_message_payload(
            &replayed_agent.instance_stream,
            SendMessagePayload {
                message: "hello after mobile reconnect".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("send mobile reconnect message");
    let event = wait_for_chat_stream_end(&mut second, "second mobile follow-up StreamEnd").await;
    let ChatEvent::StreamEnd(end) = event else {
        panic!("expected StreamEnd");
    };
    assert!(
        end.message
            .content
            .contains("mock backend response to: hello after mobile reconnect"),
        "unexpected final message: {}",
        end.message.content
    );
}

async fn connect_mobile_client(qr: &MobilePairingQrPayload) -> client::Connection {
    let mobile_stream = timeout(
        EVENT_TIMEOUT,
        mqtt_transport::connect_ephemeral(MqttConnectConfig {
            endpoint: qr.broker.clone(),
            room: qr.room,
            psk: qr.psk.clone(),
            role: ParticipantRole::Client,
        }),
    )
    .await
    .expect("timed out connecting mobile MQTT transport")
    .expect("mobile MQTT transport");
    timeout(
        EVENT_TIMEOUT,
        client::connect(&client::ClientConfig::current(), mobile_stream),
    )
    .await
    .expect("timed out waiting for mobile Tyde Hello")
    .expect("mobile Tyde Hello")
}

async fn expect_mobile_replay(
    mobile: &mut client::Connection,
    expected_projects: usize,
    context: &str,
) -> NewAgentPayload {
    let mut project_count = 0;
    let mut replayed_agent = None;
    while replayed_agent.is_none() || project_count < expected_projects {
        let env = next_event(mobile, context).await;
        match env.kind {
            FrameKind::HostBootstrap => {
                let bootstrap: HostBootstrapPayload =
                    env.parse_payload().expect("parse HostBootstrap");
                project_count = bootstrap.projects.len();
                replayed_agent = bootstrap.agents.into_iter().next();
            }
            FrameKind::CommandError => {
                let payload: CommandErrorPayload =
                    env.parse_payload().expect("parse command error");
                panic!("command error during {context}: {payload:?}");
            }
            _ => {}
        }
    }
    assert_eq!(project_count, expected_projects);
    replayed_agent.expect("replayed agent")
}
