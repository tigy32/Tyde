//! Direct HTTP hosting of the mobile web app, driven the way a real
//! deployment drives it: settings written over the protocol by a desktop
//! client, then the resulting origin fetched over real HTTP.
//!
//! The assertions are the properties a browser depends on and that a static
//! origin has historically got wrong — the `application/wasm` content type
//! that `WebAssembly.instantiateStreaming` requires, the `no-store` manifest
//! that makes revocation work, the immutable bundle cache, and the security
//! headers that only an HTTP response (not the loader's `<meta>` copy) can
//! carry.

use std::time::Duration;

use mqtt_transport::DirectMobilePairingQrPayload;
use protocol::{
    CommandErrorPayload, Envelope, FrameKind, MobileAccessStatePayload, MobileDirectHostingStatus,
    MobilePairingOfferPayload, MobilePairingStartPayload,
};
use settings_model::HostBootstrapPayload;
use tokio::time::timeout;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A bundle laid out exactly like the deployed `tyde/` prefix: loader shell and
/// manifest at the root, the immutable app bundle under `v<version>/`.
struct BundleFixture {
    dir: tempfile::TempDir,
    wasm_bytes: Vec<u8>,
}

impl BundleFixture {
    fn write() -> Self {
        let dir = tempfile::tempdir().expect("create bundle dir");
        let root = dir.path();
        // A real wasm preamble, so a content-type assertion is about a file a
        // browser would actually try to instantiate.
        let wasm_bytes = b"\0asm\x01\0\0\0extremely small module".to_vec();

        std::fs::write(
            root.join("index.html"),
            "<!doctype html><title>Tyde</title>",
        )
        .expect("write loader index");
        std::fs::write(root.join("loader.js"), "export const loader = true;\n")
            .expect("write loader script");
        std::fs::write(root.join("loader.css"), ":root { color: canvastext; }\n")
            .expect("write loader style");
        std::fs::write(
            root.join("manifest.webmanifest"),
            r#"{"name":"Tyde","display":"standalone"}"#,
        )
        .expect("write webmanifest");
        std::fs::write(
            root.join("manifest.json"),
            r#"{"schemaVersion":1,"minSupported":"0.0.1","blocked":[],"versions":{}}"#,
        )
        .expect("write manifest");

        let bundle = root.join("v9.9.9-test");
        std::fs::create_dir_all(&bundle).expect("create bundle version dir");
        std::fs::write(bundle.join("app.js"), "export default 1;\n").expect("write bundle entry");
        std::fs::write(bundle.join("app_bg.wasm"), &wasm_bytes).expect("write bundle wasm");

        Self { dir, wasm_bytes }
    }

    fn path(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }
}

struct Harness {
    host: server::HostHandle,
    _store_dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::build(None)
    }

    fn with_pairing_ttl(ttl: Duration) -> Self {
        Self::build(Some(ttl))
    }

    fn build(pairing_ttl: Option<Duration>) -> Self {
        let store_dir = tempfile::tempdir().expect("create direct hosting store dir");
        let runtime_config = server::HostRuntimeConfig {
            mobile_pairing_ttl: pairing_ttl,
            ..Default::default()
        };
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            store_dir.path().join("sessions.json"),
            store_dir.path().join("projects.json"),
            store_dir.path().join("settings.json"),
            runtime_config,
        )
        .expect("spawn test host");
        Self {
            host,
            _store_dir: store_dir,
        }
    }

    async fn connect_desktop(&self) -> client::Connection {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();
        let host = self.host.clone();

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
}

async fn next_event(client: &mut client::Connection, context: &str) -> Envelope {
    timeout(EVENT_TIMEOUT, client.next_event())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
        .unwrap_or_else(|error| panic!("failed reading {context}: {error:?}"))
        .unwrap_or_else(|| panic!("connection closed while waiting for {context}"))
}

async fn expect_initial_replay(client: &mut client::Connection) -> MobileAccessStatePayload {
    loop {
        let env = next_event(client, "initial HostBootstrap").await;
        if env.kind != FrameKind::HostBootstrap {
            continue;
        }
        let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse HostBootstrap");
        return bootstrap.mobile_access;
    }
}

async fn wait_for_direct_hosting(
    client: &mut client::Connection,
    predicate: impl Fn(&MobileDirectHostingStatus) -> bool,
    context: &str,
) -> MobileDirectHostingStatus {
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
        if predicate(&state.direct_hosting) {
            return state.direct_hosting;
        }
    }
}

async fn enable_direct_hosting(
    client: &mut client::Connection,
    bundle_dir: &str,
) -> MobileDirectHostingStatus {
    client
        .replace_setting("/enable_mobile_connections", true, false)
        .await
        .expect("set enable_mobile_connections");
    client
        .replace_setting(
            "/mobile_direct_bundle_dir",
            Some(bundle_dir),
            Option::<String>::None,
        )
        .await
        .expect("set mobile_direct_bundle_dir");
    // Port 0 lets the OS pick, so the test never collides with a real service.
    client
        .replace_setting(
            "/mobile_direct_bind_addr",
            Some("127.0.0.1:0"),
            Option::<String>::None,
        )
        .await
        .expect("set mobile_direct_bind_addr");
    client
        .replace_setting(
            "/mobile_direct_public_origin",
            Some("https://tyde.test.internal"),
            Option::<String>::None,
        )
        .await
        .expect("set mobile_direct_public_origin");
    client
        .replace_setting("/mobile_direct_hosting_enabled", true, false)
        .await
        .expect("set mobile_direct_hosting_enabled");

    wait_for_direct_hosting(
        client,
        |status| !matches!(status, MobileDirectHostingStatus::Disabled),
        "direct hosting to come up",
    )
    .await
}

fn online_addr(status: &MobileDirectHostingStatus) -> String {
    match status {
        MobileDirectHostingStatus::Online { bind_addr, .. } => bind_addr.clone(),
        other => panic!("expected direct hosting to be online, got {other:?}"),
    }
}

async fn get(base: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://{base}{path}"))
        .send()
        .await
        .unwrap_or_else(|error| panic!("GET {path} failed: {error}"))
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("response has no {name} header"))
        .to_str()
        .unwrap_or_else(|_| panic!("{name} header is not valid UTF-8"))
        .to_owned()
}

/// The whole flow: a desktop client turns direct hosting on, the host binds a
/// port and reports it, and the origin then serves the loader, the manifest and
/// the immutable bundle the way a browser needs them.
#[tokio::test]
async fn direct_hosting_serves_the_mobile_web_origin() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::new();
    let mut client = harness.connect_desktop().await;

    let initial = expect_initial_replay(&mut client).await;
    assert_eq!(
        initial.direct_hosting,
        MobileDirectHostingStatus::Disabled,
        "direct hosting must be off until it is turned on"
    );

    let status = enable_direct_hosting(&mut client, &bundle.path()).await;
    let base = online_addr(&status);

    // The loader shell: `/tyde/` resolves to its index, and `/tyde` redirects
    // there rather than leaving the loader's relative `./manifest.json` to
    // resolve one level too high.
    let shell = get(&base, "/tyde/").await;
    assert_eq!(shell.status(), 200);
    assert_eq!(header(&shell, "content-type"), "text/html; charset=utf-8");
    assert_eq!(header(&shell, "cache-control"), "public,max-age=60");

    let redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build non-redirecting client")
        .get(format!("http://{base}/tyde"))
        .send()
        .await
        .expect("GET /tyde");
    assert_eq!(redirect.status(), 308);
    assert_eq!(header(&redirect, "location"), "/tyde/");

    // The manifest is the revocation authority: a cached copy would let a
    // blocked bundle keep booting.
    let manifest = get(&base, "/tyde/manifest.json").await;
    assert_eq!(manifest.status(), 200);
    assert_eq!(header(&manifest, "content-type"), "application/json");
    assert_eq!(header(&manifest, "cache-control"), "no-store");

    // The wasm: served byte-for-byte, as `application/wasm` (any other type
    // breaks `WebAssembly.instantiateStreaming`) and cacheable forever.
    let wasm = get(&base, "/tyde/v9.9.9-test/app_bg.wasm").await;
    assert_eq!(wasm.status(), 200);
    assert_eq!(header(&wasm, "content-type"), "application/wasm");
    assert_eq!(
        header(&wasm, "cache-control"),
        "public,max-age=31536000,immutable"
    );
    assert_eq!(
        wasm.bytes().await.expect("read wasm body").to_vec(),
        bundle.wasm_bytes,
        "served wasm must be the bundle's bytes"
    );

    // Security headers ride on every response. `frame-ancestors` and the
    // camera policy exist only here — the loader's `<meta>` CSP cannot express
    // either one.
    let csp = header(&shell, "content-security-policy");
    assert!(
        csp.contains("script-src 'self' 'wasm-unsafe-eval'"),
        "CSP must allow the bundle's wasm without general unsafe-eval: {csp}"
    );
    assert!(
        csp.contains("frame-ancestors 'none'"),
        "CSP must deny framing: {csp}"
    );
    assert_eq!(header(&shell, "x-content-type-options"), "nosniff");
    assert_eq!(header(&shell, "permissions-policy"), "camera=(self)");

    // Nothing outside the served prefix exists, and a traversal attempt is
    // just another miss — requests are resolved against an in-memory map, so
    // they never reach the filesystem.
    assert_eq!(get(&base, "/").await.status(), 404);
    assert_eq!(get(&base, "/etc/passwd").await.status(), 404);
    assert_eq!(get(&base, "/tyde/../../etc/passwd").await.status(), 404);
    assert_eq!(get(&base, "/tyde/nope.js").await.status(), 404);
}

/// A bundle directory that is not a bundle must say so on the wire. Left to a
/// log line, the user sees only a Mobile tab that claims to be hosting and an
/// origin that serves nothing.
#[tokio::test]
async fn direct_hosting_reports_an_unusable_bundle_directory() {
    server::install_default_crypto_provider();
    let empty = tempfile::tempdir().expect("create empty dir");
    let harness = Harness::new();
    let mut client = harness.connect_desktop().await;
    expect_initial_replay(&mut client).await;

    let status = enable_direct_hosting(&mut client, &empty.path().to_string_lossy()).await;

    let MobileDirectHostingStatus::Error { message } = &status else {
        panic!("expected an error for a directory with no loader shell, got {status:?}");
    };
    assert!(
        message.contains("index.html"),
        "error must name what is missing: {message}"
    );
}

/// Turning direct hosting off must actually release the port, not just stop
/// advertising it.
#[tokio::test]
async fn disabling_direct_hosting_stops_serving() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::new();
    let mut client = harness.connect_desktop().await;
    expect_initial_replay(&mut client).await;

    let status = enable_direct_hosting(&mut client, &bundle.path()).await;
    let base = online_addr(&status);
    assert_eq!(get(&base, "/tyde/").await.status(), 200);

    client
        .replace_setting("/mobile_direct_hosting_enabled", false, true)
        .await
        .expect("clear mobile_direct_hosting_enabled");
    wait_for_direct_hosting(
        &mut client,
        |status| matches!(status, MobileDirectHostingStatus::Disabled),
        "direct hosting to stop",
    )
    .await;

    let stopped = timeout(EVENT_TIMEOUT, async {
        loop {
            if reqwest::Client::new()
                .get(format!("http://{base}/tyde/"))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "the origin must stop answering once direct hosting is off"
    );
}

/// A build with no bundle compiled in, pointed at no directory, has nothing to
/// serve. It has to say so and say what to do about it, rather than binding a
/// port that answers 404 for every asset the phone asks for. `./dev.sh check`
/// never embeds a bundle, so this is the shape every development build has.
#[tokio::test]
async fn direct_hosting_without_any_bundle_explains_itself() {
    server::install_default_crypto_provider();
    let harness = Harness::new();
    let mut client = harness.connect_desktop().await;
    expect_initial_replay(&mut client).await;

    client
        .replace_setting("/enable_mobile_connections", true, false)
        .await
        .expect("set enable_mobile_connections");
    client
        .replace_setting(
            "/mobile_direct_bind_addr",
            Some("127.0.0.1:0"),
            Option::<String>::None,
        )
        .await
        .expect("set mobile_direct_bind_addr");
    client
        .replace_setting("/mobile_direct_hosting_enabled", true, false)
        .await
        .expect("set mobile_direct_hosting_enabled");

    let status = wait_for_direct_hosting(
        &mut client,
        |status| !matches!(status, MobileDirectHostingStatus::Disabled),
        "direct hosting to report a missing bundle",
    )
    .await;
    let MobileDirectHostingStatus::Error { message } = status else {
        panic!("expected a reported error, got {status:?}");
    };
    assert!(
        message.contains("bundle") && message.contains("build-mobile-web-bundle.sh"),
        "the failure must name what is missing and how to produce it; got: {message:?}"
    );
}

/// `enable_mobile_connections` is documented as the master switch for mobile
/// access. Direct hosting is a second transport, so turning that switch off has
/// to take the HTTP origin down too — otherwise a paired phone keeps a working
/// route into the host after the user believes they closed mobile access.
#[tokio::test]
async fn the_mobile_master_switch_stops_direct_hosting() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::new();
    let mut client = harness.connect_desktop().await;
    expect_initial_replay(&mut client).await;

    let status = enable_direct_hosting(&mut client, &bundle.path()).await;
    let base = online_addr(&status);
    assert_eq!(get(&base, "/tyde/").await.status(), 200);

    // Only the master switch moves; direct hosting stays enabled.
    client
        .replace_setting("/enable_mobile_connections", false, true)
        .await
        .expect("clear enable_mobile_connections");
    // Direct hosting is still switched on, so going quiet would read as
    // "nothing happened" in the settings tab. The host says which switch did it.
    let stopped_status = wait_for_direct_hosting(
        &mut client,
        |status| matches!(status, MobileDirectHostingStatus::Error { .. }),
        "direct hosting to stop with the master switch",
    )
    .await;
    let MobileDirectHostingStatus::Error { message } = &stopped_status else {
        panic!("expected a reported reason, got {stopped_status:?}");
    };
    assert!(
        message.contains("mobile connections are off"),
        "the reason must name the switch that stopped it; got: {message:?}"
    );

    let stopped = timeout(EVENT_TIMEOUT, async {
        loop {
            if reqwest::Client::new()
                .get(format!("http://{base}/tyde/"))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "the origin must stop answering once mobile connections are off"
    );
}

// ── Pairing and the protocol transport ──────────────────────────────────────

/// A byte stream over a client WebSocket, mirroring the host's own adapter, so
/// the real `client::connect` handshake can run over it.
struct WebSocketClientDuplex {
    inbound: tokio::sync::mpsc::Receiver<Vec<u8>>,
    outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
    partial: Vec<u8>,
    partial_offset: usize,
}

impl tokio::io::AsyncRead for WebSocketClientDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.partial_offset >= self.partial.len() {
            match self.inbound.poll_recv(cx) {
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())),
                std::task::Poll::Ready(Some(frame)) => {
                    self.partial = frame;
                    self.partial_offset = 0;
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        let available = &self.partial[self.partial_offset..];
        let take = available.len().min(buf.remaining());
        buf.put_slice(&available[..take]);
        self.partial_offset += take;
        std::task::Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for WebSocketClientDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.outbound
            .try_send(buf.to_vec())
            .map_err(|error| std::io::Error::other(format!("websocket send failed: {error}")))?;
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Opens the host's mobile WebSocket the way the browser does: token offered as
/// a subprotocol, never as a query parameter.
async fn open_mobile_websocket(
    base: &str,
    token: &str,
) -> Result<WebSocketClientDuplex, tokio_tungstenite::tungstenite::Error> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut request = format!("ws://{base}/tyde/ws")
        .into_client_request()
        .expect("build websocket request");
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("tyde.v1, tyde.token.{token}")).expect("subprotocol header"),
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut sink, mut stream) = socket.split();

    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(32);
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            if sink
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    bytes.into(),
                ))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message
                && inbound_tx.send(bytes.into()).await.is_err()
            {
                return;
            }
        }
    });

    Ok(WebSocketClientDuplex {
        inbound: inbound_rx,
        outbound: outbound_tx,
        partial: Vec::new(),
        partial_offset: 0,
    })
}

async fn start_direct_pairing(client: &mut client::Connection) -> MobilePairingOfferPayload {
    send_host_payload(
        client,
        FrameKind::MobilePairingStart,
        &MobilePairingStartPayload { direct: true },
    )
    .await;
    loop {
        let env = next_event(client, "direct pairing offer").await;
        if env.kind == FrameKind::CommandError {
            let payload: CommandErrorPayload = env.parse_payload().expect("parse command error");
            panic!("command error while starting direct pairing: {payload:?}");
        }
        if env.kind == FrameKind::MobilePairingOffer {
            return env.parse_payload().expect("parse MobilePairingOffer");
        }
    }
}

async fn send_host_payload<T: serde::Serialize>(
    client: &mut client::Connection,
    kind: FrameKind,
    payload: &T,
) {
    let stream = client
        .outgoing_seq
        .keys()
        .find(|stream| stream.0.starts_with("/host/"))
        .cloned()
        .expect("missing host stream");
    let seq = client
        .outgoing_seq
        .get(&stream)
        .copied()
        .expect("missing host stream sequence counter");
    let envelope = protocol::Envelope::from_payload(stream.clone(), kind, seq, payload)
        .expect("serialize host payload");
    client.outgoing_seq.insert(stream, seq + 1);
    protocol::write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write payload");
}

/// Extracts the `tyde-pair://v3?…` URI the host put in the QR link's fragment.
fn pairing_uri_fragment(qr_uri: &str) -> &str {
    qr_uri
        .split_once('#')
        .map(|(_, fragment)| fragment)
        .unwrap_or_else(|| panic!("pairing URL {qr_uri} carries no fragment"))
}

async fn redeem(base: &str, payload: &DirectMobilePairingQrPayload) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{base}/tyde/pair"))
        .json(&protocol::MobileDirectPairRequest {
            offer_id: payload.offer_id.clone(),
            offer_secret: payload.offer_secret.clone(),
            device_label: "Test Phone".to_owned(),
        })
        .send()
        .await
        .expect("POST /tyde/pair")
}

/// The pairing and transport flow end to end: the host mints an offer, a phone
/// redeems it over HTTP for a durable token, connects a WebSocket with that
/// token, completes the real protocol handshake, and drives an agent.
#[tokio::test]
async fn direct_pairing_grants_a_working_protocol_connection() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::new();
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let status = enable_direct_hosting(&mut desktop, &bundle.path()).await;
    let base = online_addr(&status);

    let offer = start_direct_pairing(&mut desktop).await;
    let payload = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&offer.qr_uri.0))
        .expect("parse the direct pairing QR");
    assert_eq!(
        payload.offer_id, offer.offer_id,
        "the QR must describe the offer the host announced"
    );

    let response = redeem(&base, &payload).await;
    assert_eq!(response.status(), 200, "redeeming a fresh offer must work");
    let grant: protocol::MobileDirectPairResponse =
        response.json().await.expect("parse pair response");

    // Single use: the same secret must not mint a second device.
    let replay = redeem(&base, &payload).await;
    assert_eq!(
        replay.status(),
        403,
        "a redeemed pairing code must not work twice"
    );

    // The device now appears in the host's paired list.
    let state = wait_for_direct_hosting(
        &mut desktop,
        |status| matches!(status, MobileDirectHostingStatus::Online { .. }),
        "device to be recorded",
    )
    .await;
    assert!(matches!(state, MobileDirectHostingStatus::Online { .. }));

    // The token opens a real protocol connection.
    let duplex = open_mobile_websocket(&base, &grant.device_token.0)
        .await
        .expect("open the mobile websocket");
    let mut phone = client::connect(&client::ClientConfig::current(), duplex)
        .await
        .expect("mobile handshake over the websocket");

    let bootstrap = loop {
        let env = next_event(&mut phone, "phone HostBootstrap").await;
        if env.kind == FrameKind::HostBootstrap {
            break env;
        }
    };
    let bootstrap: HostBootstrapPayload = bootstrap
        .parse_payload()
        .expect("parse phone HostBootstrap");
    assert_eq!(
        bootstrap.mobile_access.paired_devices.len(),
        1,
        "the phone must see itself in the host's device list"
    );
    assert_eq!(
        bootstrap.mobile_access.paired_devices[0].device_id,
        grant.device_id
    );
}

/// An unpaired or revoked token must not reach the protocol at all.
#[tokio::test]
async fn direct_websocket_refuses_an_unknown_device_token() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::new();
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let status = enable_direct_hosting(&mut desktop, &bundle.path()).await;
    let base = online_addr(&status);

    let refused = open_mobile_websocket(&base, "not-a-real-device-token").await;
    assert!(
        refused.is_err(),
        "an unknown token must not be upgraded to a protocol connection"
    );

    // And with no token at all.
    let anonymous = tokio_tungstenite::connect_async(format!("ws://{base}/tyde/ws")).await;
    assert!(
        anonymous.is_err(),
        "an unauthenticated upgrade must be refused"
    );
}

/// An expired offer must not be redeemable, even with the right secret.
#[tokio::test]
async fn direct_pairing_offer_expires() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let harness = Harness::with_pairing_ttl(Duration::from_millis(150));
    let mut desktop = harness.connect_desktop().await;
    expect_initial_replay(&mut desktop).await;

    let status = enable_direct_hosting(&mut desktop, &bundle.path()).await;
    let base = online_addr(&status);

    let offer = start_direct_pairing(&mut desktop).await;
    let payload = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&offer.qr_uri.0))
        .expect("parse the direct pairing QR");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let response = redeem(&base, &payload).await;
    assert_eq!(
        response.status(),
        403,
        "an expired pairing code must be refused"
    );
}
