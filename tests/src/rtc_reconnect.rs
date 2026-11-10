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
    MobilePeerRole, MobileRtcCredentialsRequest, ToolPolicy,
};
use tokio::sync::{mpsc, oneshot};

enum ScenarioCommand {
    Configure(String, String),
    Read(String, bool, oneshot::Sender<(String, usize)>),
}

struct Scenario {
    mode: String,
    requests: usize,
}

struct Fixture {
    relay: Arc<crate::rtc::RelayFixture>,
    scenarios: mpsc::UnboundedSender<ScenarioCommand>,
    host: server::HostHandle,
    store: tempfile::TempDir,
    dev_host_addr: Option<std::net::SocketAddr>,
}

pub fn router(relay: Arc<crate::rtc::RelayFixture>) -> Router {
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
            hidden: false,
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
    let (scenarios, mut commands) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut scenarios = HashMap::<String, Scenario>::new();
        while let Some(command) = commands.recv().await {
            match command {
                ScenarioCommand::Configure(id, mode) => {
                    scenarios
                        .entry(id)
                        .or_insert(Scenario {
                            mode: String::new(),
                            requests: 0,
                        })
                        .mode = mode;
                }
                ScenarioCommand::Read(id, increment, reply) => {
                    let scenario = scenarios
                        .get_mut(&id)
                        .expect("configured reconnect scenario");
                    scenario.requests += usize::from(increment);
                    let _ = reply.send((scenario.mode.clone(), scenario.requests));
                }
            }
        }
    });
    Router::new()
        .route("/reconnect/{id}/control", post(control).options(preflight))
        .route("/reconnect/{id}/requests", get(requests))
        .route(
            "/reconnect/{id}/pairings/{pairing}/webrtc",
            post(credentials).options(preflight),
        )
        .with_state(Arc::new(Fixture {
            relay,
            scenarios,
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
    fixture
        .scenarios
        .send(ScenarioCommand::Configure(id, mode))
        .expect("scenario actor");
    StatusCode::NO_CONTENT
}

async fn requests(State(fixture): State<Arc<Fixture>>, Path(id): Path<String>) -> Json<usize> {
    let (reply, result) = oneshot::channel();
    fixture
        .scenarios
        .send(ScenarioCommand::Read(id, false, reply))
        .expect("scenario actor");
    let (_, count) = result.await.expect("scenario count");
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
    let (reply, result) = oneshot::channel();
    fixture
        .scenarios
        .send(ScenarioCommand::Read(id.clone(), true, reply))
        .expect("scenario actor");
    let (mode, count) = result.await.expect("scenario credentials");
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
        "ready" => {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("push listener");
            let base = format!("http://{}", listener.local_addr().expect("push address"));
            let (signal, router) = crate::rtc::PushSignaling::router(&base);
            let service = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("push service");
            });
            let host_credentials =
                signal.credentials(MobilePeerRole::Host, fixture.relay.tls_ice.clone());
            let roots = fixture.relay.roots.clone();
            let mut mobile_credentials =
                signal.credentials(MobilePeerRole::Mobile, fixture.relay.browser_ice.clone());
            mobile_credentials.pairing_id = MobilePairingId(pairing);
            let connected = signal.clone();
            tokio::spawn(async move {
                let flow = async {
                    let stream = rtc_transport::connect_with_tls_roots(
                        host_credentials,
                        &[53; 32],
                        |_| {},
                        roots,
                    )
                    .await
                    .map_err(std::io::Error::other)?;
                    service.abort();
                    assert!(
                        fixture.store.path().exists(),
                        "disposable store remains alive"
                    );
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
                        MobileDeviceId("rtc-reconnect".into()),
                    )
                    .await
                    .map_err(std::io::Error::other)
                };
                if let Err(error) = flow.await {
                    eprintln!("reconnect fixture: host connection ended: {error}");
                }
            });
            connected.host_connected_after(0).await;
            Json(mobile_credentials).into_response()
        }
        _ => panic!("unexpected reconnect fixture mode"),
    }
}
