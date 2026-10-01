use std::{collections::HashMap, sync::Arc, time::SystemTime};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use protocol::{
    CustomAgent, CustomAgentId, MOBILE_RTC_PROTOCOL_VERSION, MobileDeviceId, MobilePairingId,
    MobilePeerRole, MobileRtcCredentials, MobileRtcCredentialsRequest, MobileRtcDescription,
    MobileSdpKind, MobileSignalCommand, MobileSignalEvent, MobileSignalingUrl, ToolPolicy,
};
use rtc_transport::{Peer, authenticate_description, verify_description};
use tokio::sync::Mutex;

struct Scenario {
    mode: String,
    requests: usize,
}

struct Fixture {
    relay: Arc<crate::rtc::RelayFixture>,
    base_url: String,
    scenarios: Mutex<HashMap<String, Scenario>>,
    answers: Mutex<HashMap<String, MobileRtcDescription>>,
    host: server::HostHandle,
    store: tempfile::TempDir,
    dev_host_addr: Option<std::net::SocketAddr>,
}

pub fn router(relay: Arc<crate::rtc::RelayFixture>, base_url: String) -> Router {
    let dev_host_addr = std::env::var_os("TYDE_RTC_TEST_DEV_HOST_ADDR").map(|value| {
        let address: std::net::SocketAddr = value
            .to_str()
            .expect("dev host address must be UTF-8")
            .parse()
            .expect("typed dev-instance host address");
        assert!(address.ip().is_loopback(), "dev host must be loopback");
        address
    });
    let store = tempfile::tempdir().expect("reconnect host store");
    server::store::custom_agents::CustomAgentStore::load(store.path().join("custom_agents.json"))
        .expect("reconnect custom agents")
        .upsert(CustomAgent {
            id: CustomAgentId("rtc-bootstrap".to_owned()),
            name: "RTC bootstrap agent".to_owned(),
            description: "Initial state spans multiple acknowledgement windows".to_owned(),
            instructions: Some("Follow the project instructions.\n".repeat(4096)),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        })
        .expect("seed reconnect initial state");
    let host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .expect("reconnect host");
    Router::new()
        .route("/reconnect/{id}/control", post(control).options(preflight))
        .route("/reconnect/{id}/requests", get(requests))
        .route(
            "/reconnect/{id}/pairings/{pairing}/webrtc",
            post(credentials).options(preflight),
        )
        .route("/reconnect/{id}/signal", post(signal).options(preflight))
        .with_state(Arc::new(Fixture {
            relay,
            base_url,
            scenarios: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            host,
            store,
            dev_host_addr,
        }))
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn control(
    State(fixture): State<Arc<Fixture>>,
    Path(id): Path<String>,
    mode: String,
) -> StatusCode {
    let mut scenarios = fixture.scenarios.lock().await;
    scenarios
        .entry(id)
        .or_insert(Scenario {
            mode: String::new(),
            requests: 0,
        })
        .mode = mode;
    StatusCode::NO_CONTENT
}

async fn requests(State(fixture): State<Arc<Fixture>>, Path(id): Path<String>) -> Json<usize> {
    let count = fixture
        .scenarios
        .lock()
        .await
        .get(&id)
        .expect("scenario")
        .requests;
    eprintln!(
        "reconnect fixture: observed count={count} time={:?}",
        SystemTime::now()
    );
    Json(count)
}

async fn credentials(
    State(fixture): State<Arc<Fixture>>,
    Path((id, pairing)): Path<(String, String)>,
    Json(request): Json<MobileRtcCredentialsRequest>,
) -> Response {
    assert_eq!(request.protocol_version, MOBILE_RTC_PROTOCOL_VERSION);
    assert_eq!(request.role, MobilePeerRole::Mobile);
    let (mode, count) = {
        let mut scenarios = fixture.scenarios.lock().await;
        let scenario = scenarios
            .get_mut(&id)
            .expect("configured reconnect scenario");
        scenario.requests += 1;
        (scenario.mode.clone(), scenario.requests)
    };
    eprintln!(
        "reconnect fixture: credential request mode={mode} count={count} time={:?}",
        SystemTime::now()
    );
    match mode.as_str() {
        "stall" => std::future::pending::<Response>().await,
        "stall-body" => Response::new(Body::from_stream(
            futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"{")) })
                .chain(futures_util::stream::pending()),
        )),
        "unavailable" => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":{
                "code":"service_unavailable", "message":"Relay service temporarily unavailable",
                "retryable":true
            }})),
        )
            .into_response(),
        "ready" => Json(MobileRtcCredentials {
            protocol_version: MOBILE_RTC_PROTOCOL_VERSION,
            pairing_id: MobilePairingId(pairing),
            role: MobilePeerRole::Mobile,
            signaling_url: MobileSignalingUrl(format!(
                "{}/reconnect/{id}/signal",
                fixture.base_url
            )),
            signaling_token: "local-test-only".to_owned(),
            ice_servers: vec![fixture.relay.browser_ice.clone()],
            expires_at_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_millis() as u64
                + 3_600_000,
        })
        .into_response(),
        _ => panic!("unexpected reconnect fixture mode"),
    }
}

async fn signal(
    State(fixture): State<Arc<Fixture>>,
    Json(command): Json<MobileSignalCommand>,
) -> Json<MobileSignalEvent> {
    match command {
        MobileSignalCommand::Publish { description } => {
            let session = description.session_id.clone();
            verify_description(&description, &session, MobileSdpKind::Offer, &[53; 32])
                .expect("authenticated reconnect offer");
            let mut peer = Peer::with_tls_roots(
                std::slice::from_ref(&fixture.relay.tls_ice),
                fixture.relay.roots.clone(),
            )
            .await
            .expect("native reconnect peer");
            let sdp = peer
                .answer(description.sdp)
                .await
                .expect("native reconnect answer");
            let answer =
                authenticate_description(session.clone(), MobileSdpKind::Answer, sdp, &[53; 32])
                    .expect("signed reconnect answer");
            fixture.answers.lock().await.insert(session.0, answer);
            tokio::spawn(async move {
                assert!(
                    fixture.store.path().exists(),
                    "disposable store remains alive"
                );
                let flow = async {
                    let stream = peer.into_stream().await.map_err(std::io::Error::other)?;
                    if let Some(address) = fixture.dev_host_addr {
                        let mut stream = stream;
                        let mut host = tokio::net::TcpStream::connect(address).await?;
                        tokio::io::copy_bidirectional(&mut stream, &mut host).await?;
                        return Ok(());
                    }
                    let connection = server::accept(&server::ServerConfig::current(), stream)
                        .await
                        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
                    server::run_mobile_connection(
                        connection,
                        fixture.host.clone(),
                        MobileDeviceId("rtc-reconnect".to_owned()),
                    )
                    .await
                    .map_err(std::io::Error::other)
                };
                if let Err(error) = flow.await {
                    eprintln!("reconnect fixture: host connection ended: {error}");
                }
            });
            Json(MobileSignalEvent::Waiting)
        }
        MobileSignalCommand::Poll { session_id } => {
            let session = session_id.expect("mobile polls its session");
            Json(match fixture.answers.lock().await.get(session.as_str()) {
                Some(description) => MobileSignalEvent::Description {
                    description: description.clone(),
                },
                None => MobileSignalEvent::Waiting,
            })
        }
    }
}
