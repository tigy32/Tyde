//! Mobile push notifications, end to end: a paired device registers a Web Push
//! subscription over the real protocol from a real mobile-origin connection, an
//! agent finishes a turn, and the host delivers an encrypted notification to the
//! subscription's endpoint.
//!
//! The endpoint is an ordinary HTTP server this test runs, reached because the
//! endpoint URL is client-supplied data — no production code branches for the
//! test. The captured body is decrypted with the subscription's own private key,
//! so this covers the RFC 8291 encryption and RFC 8292 VAPID authorization the
//! host performs, not merely that a request was made.

mod fixture;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes128Gcm, Key, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fixture::Fixture;
use hkdf::Hkdf;
use mqtt_transport::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use protocol::{
    BrokerUrl, FrameKind, MobileDeviceId, MobileDeviceState, MobilePushNotification,
    MobilePushReason, MobilePushSubscribePayload, MobilePushSubscription, PushAuthSecret,
    PushEndpointUrl, PushPublicKey, VapidPrivateKey, VapidPublicKey,
};
use server::backend::mock::{MockScript, MockTurn};
use server::store::mobile_pairings::{
    MOBILE_PAIRINGS_STORE_PATH_ENV, MobilePairingRecord, MobilePairings, MobilePairingsStore,
    key_fingerprint,
};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const DEVICE_ID: &str = "test-device";
const PUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Everything the device half of a Web Push subscription holds.
struct DeviceKeys {
    secret: SecretKey,
    public: Vec<u8>,
    auth: [u8; 16],
    vapid_public: String,
    vapid_private: String,
}

impl DeviceKeys {
    fn generate() -> Self {
        let secret = SecretKey::random(&mut OsRng);
        let public = secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        let mut auth = [0u8; 16];
        OsRng.fill_bytes(&mut auth);
        let vapid = SecretKey::random(&mut OsRng);
        Self {
            secret,
            public,
            auth,
            vapid_public: URL_SAFE_NO_PAD
                .encode(vapid.public_key().to_encoded_point(false).as_bytes()),
            vapid_private: URL_SAFE_NO_PAD.encode(vapid.to_bytes()),
        }
    }

    fn subscription(&self, endpoint: String) -> MobilePushSubscription {
        MobilePushSubscription {
            endpoint: PushEndpointUrl(endpoint),
            p256dh: PushPublicKey(URL_SAFE_NO_PAD.encode(&self.public)),
            auth: PushAuthSecret(URL_SAFE_NO_PAD.encode(self.auth)),
            vapid_public_key: VapidPublicKey(self.vapid_public.clone()),
            vapid_private_key: VapidPrivateKey(self.vapid_private.clone()),
        }
    }

    /// Inverse of the host's `aes128gcm` encryption (RFC 8188 §2, RFC 8291 §3.4).
    fn decrypt(&self, body: &[u8]) -> Vec<u8> {
        assert!(
            body.len() > 21,
            "push body is too short to be a valid record"
        );
        let salt = &body[0..16];
        let key_len = usize::from(body[20]);
        let as_public = &body[21..21 + key_len];
        let ciphertext = &body[21 + key_len..];

        let as_key = PublicKey::from_sec1_bytes(as_public).expect("server ephemeral key");
        let shared =
            p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), as_key.as_affine());

        let mut key_info = Vec::new();
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(&self.public);
        key_info.extend_from_slice(as_public);

        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&self.auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .expect("expand ikm");

        let hkdf = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        hkdf.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .expect("expand cek");
        let mut nonce = [0u8; 12];
        hkdf.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .expect("expand nonce");

        let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
        let mut plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: b"",
                },
            )
            .expect("decrypt push payload");
        // Strip the RFC 8188 padding delimiter.
        assert_eq!(
            plaintext.pop(),
            Some(0x02),
            "expected a last-record delimiter"
        );
        plaintext
    }
}

struct CapturedPush {
    authorization: String,
    content_encoding: String,
    body: Vec<u8>,
}

/// A minimal stand-in for a push service: accepts one POST per connection,
/// replies with `status`, and reports what it received.
async fn spawn_push_endpoint(status: &'static str) -> (String, mpsc::Receiver<CapturedPush>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind push endpoint");
    let addr = listener.local_addr().expect("push endpoint addr");
    let (tx, rx) = mpsc::channel(8);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until the declared body has arrived.
                loop {
                    let read = match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    buffer.extend_from_slice(&chunk[..read]);
                    let Some(split) = find_header_end(&buffer) else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&buffer[..split]).to_string();
                    let content_length = header_value(&headers, "content-length")
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if buffer.len() - split - 4 < content_length {
                        continue;
                    }
                    let body = buffer[split + 4..split + 4 + content_length].to_vec();
                    let _ = tx
                        .send(CapturedPush {
                            authorization: header_value(&headers, "authorization")
                                .unwrap_or_default(),
                            content_encoding: header_value(&headers, "content-encoding")
                                .unwrap_or_default(),
                            body,
                        })
                        .await;
                    let _ = socket
                        .write_all(
                            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                        )
                        .await;
                    let _ = socket.flush().await;
                    break;
                }
            });
        }
    });

    (format!("http://127.0.0.1:{}/push", addr.port()), rx)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim().eq_ignore_ascii_case(name)).then(|| value.trim().to_owned())
    })
}

/// Seeds one paired device before the host starts, so the mobile access actor
/// loads it. Returns the guard that keeps the temp dir alive.
fn seed_paired_device() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("pairings tempdir");
    let path = dir.path().join("mobile_pairings.json");
    // SAFETY: nextest runs each test in its own process, so this does not race
    // another test's environment.
    unsafe {
        std::env::set_var(MOBILE_PAIRINGS_STORE_PATH_ENV, &path);
    }

    let store = MobilePairingsStore::load(path).expect("load pairings store");
    let psk = PreSharedKey::random();
    let mut pairings = MobilePairings::empty();
    pairings.devices.push(MobilePairingRecord {
        device_id: MobileDeviceId(DEVICE_ID.to_owned()),
        broker: BrokerEndpoint {
            url: BrokerUrl::new("wss://broker.invalid:8084/mqtt").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        },
        room: RoomId::random(),
        key_fingerprint: key_fingerprint(&psk),
        psk,
        label: "Test phone".to_owned(),
        created_at_ms: 1,
        last_seen_at_ms: None,
        state: MobileDeviceState::Paired,
        push: None,
        managed: None,
    });
    store.save(&pairings).expect("seed pairings store");
    dir
}

/// Connects as the paired mobile device over the real mobile connection path.
async fn connect_mobile(host: server::HostHandle) -> client::Connection {
    fixture::connect_raw_mobile_client(host, DEVICE_ID).await
}

async fn register_subscription(
    mobile: &mut client::Connection,
    subscription: MobilePushSubscription,
) {
    mobile
        .mobile_push_subscribe(MobilePushSubscribePayload { subscription })
        .await
        .expect("send MobilePushSubscribe");
}

#[tokio::test]
async fn idle_agent_delivers_an_encrypted_push_to_the_paired_device() {
    let _pairings_dir = seed_paired_device();
    let (endpoint, mut pushes) = spawn_push_endpoint("201 Created").await;

    let mut fixture = Fixture::new().await;
    enable_mobile_access(&mut fixture).await;
    let keys = DeviceKeys::generate();

    let mut mobile = connect_mobile(fixture.host_for_test()).await;
    register_subscription(&mut mobile, keys.subscription(endpoint)).await;

    fixture
        .spawn_scripted(
            "push-idle",
            MockScript::one(MockTurn::text("mock backend response to: push idle")),
        )
        .await;

    let captured = tokio::time::timeout(PUSH_WAIT, pushes.recv())
        .await
        .expect("timed out waiting for a push delivery")
        .expect("push endpoint closed without delivering");

    assert_eq!(
        captured.content_encoding, "aes128gcm",
        "the host must use the RFC 8188 aes128gcm content encoding"
    );
    assert!(
        captured.authorization.starts_with("vapid t="),
        "push must carry VAPID authorization, got {:?}",
        captured.authorization
    );
    assert!(
        captured
            .authorization
            .contains(&format!("k={}", keys.vapid_public)),
        "VAPID authorization must name the key the subscription was created with"
    );

    let plaintext = keys.decrypt(&captured.body);
    let notification: MobilePushNotification =
        serde_json::from_slice(&plaintext).expect("decrypted push body is a notification");
    assert_eq!(notification.agent_name, "push-idle");
    assert_eq!(notification.reason, MobilePushReason::TurnComplete);
}

#[tokio::test]
async fn a_gone_subscription_is_recorded_as_expired() {
    let _pairings_dir = seed_paired_device();
    let (endpoint, mut pushes) = spawn_push_endpoint("410 Gone").await;

    let mut fixture = Fixture::new().await;
    enable_mobile_access(&mut fixture).await;
    let keys = DeviceKeys::generate();

    let mut mobile = connect_mobile(fixture.host_for_test()).await;
    register_subscription(&mut mobile, keys.subscription(endpoint)).await;

    fixture
        .spawn_scripted(
            "push-gone",
            MockScript::one(MockTurn::text("mock backend response to: push gone")),
        )
        .await;

    tokio::time::timeout(PUSH_WAIT, pushes.recv())
        .await
        .expect("timed out waiting for a push delivery")
        .expect("push endpoint closed without delivering");

    // The rejection must reach the device list rather than being swallowed: a
    // dead subscription that still reads as "on" is exactly the silent failure
    // this guards against. Read it from the state the server broadcasts.
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "MobileAccessState marking the subscription expired",
        |env| {
            if env.kind != FrameKind::MobileAccessState {
                return false;
            }
            let payload: protocol::MobileAccessStatePayload =
                env.parse_payload().expect("parse MobileAccessStatePayload");
            payload.paired_devices.iter().any(|device| {
                device.device_id.0 == DEVICE_ID && device.push == protocol::MobilePushState::Expired
            })
        },
    )
    .await;

    drop(mobile);
}

/// A device could only ever have paired while mobile access was on, and the
/// host does not notify a device it cannot serve, so every push test turns it on
/// the way a user would.
async fn enable_mobile_access(fixture: &mut Fixture) {
    let write_id = protocol::SettingsWriteId("enable-mobile".to_owned());
    fixture
        .client
        .settings_write(protocol::SettingsWritePayload {
            write_id: write_id.clone(),
            ops: vec![protocol::SettingOp::Replace {
                path: "/enable_mobile_connections".to_owned(),
                value: serde_json::json!(true),
                expected: protocol::SettingExpectation::Value {
                    value: serde_json::json!(false),
                },
            }],
        })
        .await
        .expect("send settings_write enabling mobile access");

    fixture::expect_settings_write_applied(&mut fixture.client, &write_id, "enable-mobile").await;
}

/// The MCP transport answers either plain JSON or a single SSE event, depending
/// on negotiation.
fn parse_mcp_body(body: &str) -> serde_json::Value {
    let payload = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or(body);
    serde_json::from_str(payload)
        .unwrap_or_else(|error| panic!("MCP body is not JSON ({error}): {body}"))
}

/// Spawns a child agent through the real agent-control MCP, the path that makes
/// it `AgentOrigin::AgentControl`.
async fn mcp_spawn_agent(caller: &server::AgentControlMcpCaller, name: &str) {
    let response = reqwest::Client::new()
        .post(&caller.url)
        .header("Authorization", &caller.authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_spawn_agent",
                "arguments": {
                    "workspace_roots": ["/tmp/mobile-push-origin"],
                    "prompt": "orchestrated work",
                    "name": name,
                    "backend_kind": "claude",
                }
            }
        }))
        .send()
        .await
        .expect("agent-control MCP request")
        .text()
        .await
        .map(|body| parse_mcp_body(&body))
        .expect("agent-control MCP response");

    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("MCP response missing result: {response}"));
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    assert!(!is_error, "tyde_spawn_agent failed: {response}");
}

/// Orchestrated agents must not notify: a workflow or team fanning out to a
/// dozen sub-agents would otherwise buzz the phone a dozen times for work the
/// user never personally started. The paired user-origin spawn is the control —
/// without it, this test would also pass if pushes were broken outright.
#[tokio::test]
async fn only_agents_the_user_started_notify() {
    let _pairings_dir = seed_paired_device();
    let (endpoint, mut pushes) = spawn_push_endpoint("201 Created").await;

    let mut fixture = Fixture::new().await;
    enable_mobile_access(&mut fixture).await;
    let keys = DeviceKeys::generate();

    let mut mobile = connect_mobile(fixture.host_for_test()).await;
    register_subscription(&mut mobile, keys.subscription(endpoint)).await;

    // A parent agent spawns a child through the agent-control MCP, which is
    // what makes the child `AgentOrigin::AgentControl`. Its own idle turn is the
    // user-started control below, so it is spawned first and drained.
    let parent = fixture
        .spawn_scripted(
            "orchestrator",
            MockScript::one(MockTurn::text("mock backend response to: orchestrate")),
        )
        .await;
    let parent_id = parent.new_agent.agent_id.clone();
    let first = tokio::time::timeout(PUSH_WAIT, pushes.recv())
        .await
        .expect("timed out waiting for the orchestrator's own push")
        .expect("push endpoint closed");
    let first: MobilePushNotification =
        serde_json::from_slice(&keys.decrypt(&first.body)).expect("notification");
    assert_eq!(first.agent_name, "orchestrator");

    let caller = fixture.agent_control_caller(&parent_id).await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "orchestrated",
            MockScript::one(MockTurn::text("mock backend response to: orchestrated")),
        )
        .await;
    mcp_spawn_agent(&caller, "orchestrated").await;
    drop(reservation);

    // Now a second spawn the user made themselves. Its notification is what
    // proves the delivery path was working the whole time.
    fixture
        .spawn_scripted(
            "user-started",
            MockScript::one(MockTurn::text("mock backend response to: user started")),
        )
        .await;

    let captured = tokio::time::timeout(PUSH_WAIT, pushes.recv())
        .await
        .expect("timed out waiting for the user-started agent's push")
        .expect("push endpoint closed without delivering");
    let notification: MobilePushNotification =
        serde_json::from_slice(&keys.decrypt(&captured.body)).expect("notification");

    assert_eq!(
        notification.agent_name, "user-started",
        "the first and only push must be for the user's own agent; an orchestrated \
         agent's idle turn must not notify"
    );

    // And nothing follows it.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), pushes.recv())
            .await
            .is_err(),
        "an orchestrated agent going idle must not deliver a second push"
    );

    drop(mobile);
}
