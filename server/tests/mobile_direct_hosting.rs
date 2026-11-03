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

mod fixture;

use fixture::Fixture;
use std::time::Duration;

use mobile_pairing::DirectMobilePairingQrPayload;
use protocol::{
    CommandErrorPayload, Envelope, FrameKind, MobileAccessStatePayload, MobileDirectHostingStatus,
    MobilePairingOfferPayload, MobilePairingStartPayload, MobilePairingState,
};
use settings_model::HostBootstrapPayload;

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

async fn next_event(client: &mut client::Connection, context: &str) -> Envelope {
    // These are positive local protocol waits, not timed product contracts.
    // The old five-second cutoff rejected healthy initial backend discovery.
    let started = std::time::Instant::now();
    let event = client
        .next_event()
        .await
        .unwrap_or_else(|error| panic!("failed reading {context}: {error:?}"))
        .unwrap_or_else(|| panic!("connection closed while waiting for {context}"));
    eprintln!(
        "MOBILE HOSTING: context={context} kind={:?} elapsed_ms={}",
        event.kind,
        started.elapsed().as_millis()
    );
    event
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
    let mut fixture = Fixture::new().await;
    let initial = fixture.bootstrap.mobile_access.clone();
    let client = &mut fixture.client;

    assert_eq!(
        initial.direct_hosting,
        MobileDirectHostingStatus::Disabled,
        "direct hosting must be off until it is turned on"
    );

    let status = enable_direct_hosting(client, &bundle.path()).await;
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
    let mut fixture = Fixture::new().await;
    let client = &mut fixture.client;

    let status = enable_direct_hosting(client, &empty.path().to_string_lossy()).await;

    let MobileDirectHostingStatus::Error { message } = &status else {
        panic!("expected an error for a directory with no loader shell, got {status:?}");
    };
    assert!(
        message.contains("index.html"),
        "error must name what is missing: {message}"
    );
}

/// A build with no bundle compiled in, pointed at no directory, has nothing to
/// serve. It has to say so and say what to do about it, rather than binding a
/// port that answers 404 for every asset the phone asks for. `./dev.sh check`
/// never embeds a bundle, so this is the shape every development build has.
#[tokio::test]
async fn direct_hosting_without_any_bundle_explains_itself() {
    server::install_default_crypto_provider();
    let mut fixture = Fixture::new().await;
    let client = &mut fixture.client;

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
        client,
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
        .unwrap_or_else(|| panic!("pairing URL carries no fragment"))
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

// The next operation replies from the same mobile actor after earlier commands.
// Observe its public error boundary, rather than sleeping for cancellation.
async fn mobile_access_events_after_commands(
    client: &mut client::Connection,
) -> Vec<MobileAccessStatePayload> {
    send_host_payload(
        client,
        FrameKind::MobileDeviceRename,
        &protocol::MobileDeviceRenamePayload {
            device_id: protocol::MobileDeviceId("unpaired-observer".into()),
            label: "Unused".into(),
        },
    )
    .await;
    // A legitimate terminal event can reach Idle after its250ms grace.
    // Keep the delivered event, not a load-dependent last-state sample.
    let mut events = Vec::new();
    loop {
        let frame = next_event(client, "mobile command completion").await;
        match frame.kind {
            FrameKind::MobileAccessState => {
                events.push(frame.parse_payload::<MobileAccessStatePayload>().unwrap());
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = frame.parse_payload().unwrap();
                assert!(error.operation == "mobile_device_rename" && !error.fatal);
                return events;
            }
            FrameKind::MobilePairingOffer => {
                panic!("unexpected pairing offer while awaiting mobile command completion");
            }
            _ => {}
        }
    }
}

async fn scheduled_direct_expiry_allows_renewal() {
    let bundle = BundleFixture::write();
    // Inject the exact expiry boundary; no elapsed-time assumption or real sleep.
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        mobile_pairing_ttl: Some(Duration::ZERO),
        ..Default::default()
    })
    .await;
    let desktop = &mut fixture.client;
    let status = enable_direct_hosting(desktop, &bundle.path()).await;
    let base = online_addr(&status);
    for _ in 0..2 {
        let offer = start_direct_pairing(desktop).await;
        let payload = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&offer.qr_uri.0))
            .expect("parse direct offer");
        loop {
            let frame = next_event(desktop, "scheduled direct expiry before redemption").await;
            assert_ne!(frame.kind, FrameKind::CommandError);
            if frame.kind == FrameKind::MobileAccessState {
                let state: MobileAccessStatePayload = frame.parse_payload().unwrap();
                if matches!(&state.pairing, MobilePairingState::Expired { offer_id } if *offer_id == offer.offer_id)
                {
                    assert!(state.paired_devices.is_empty());
                    break;
                }
            }
        }
        // HTTP cannot be the operation that repairs stale Active state.
        assert!(
            redeem(&base, &payload).await.status() == 403,
            "The server must reject an offer already expired by its scheduler"
        );
    }
}

/// The pairing and transport flow end to end: the host mints an offer, a phone
/// redeems it over HTTP for a durable token, connects a WebSocket with that
/// token, completes the real protocol handshake, and drives an agent.
#[tokio::test]
async fn direct_pairing_grants_a_working_protocol_connection() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let mut fixture = Fixture::new().await;
    let pairings_path = server::store::mobile_pairings::MobilePairingsStore::path_for_store_parent(
        fixture.store_dir(),
    );
    assert!(pairings_path.starts_with(fixture.store_dir()));
    let desktop = &mut fixture.client;

    let status = enable_direct_hosting(desktop, &bundle.path()).await;
    let base = online_addr(&status);

    let cancelled = start_direct_pairing(desktop).await;
    let cancelled_payload =
        DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&cancelled.qr_uri.0))
            .expect("parse cancelled direct offer");
    let preserved_path = pairings_path.with_extension("before-cancel.json");
    std::fs::rename(&pairings_path, &preserved_path).unwrap();
    // An actual filesystem replacement error, independent of permission bits/root.
    std::fs::create_dir(&pairings_path).unwrap();
    send_host_payload(
        desktop,
        FrameKind::MobilePairingCancel,
        &protocol::MobilePairingCancelPayload {
            offer_id: cancelled.offer_id.clone(),
        },
    )
    .await;
    let failed = mobile_access_events_after_commands(desktop).await;
    std::fs::remove_dir(&pairings_path).unwrap();
    std::fs::rename(&preserved_path, &pairings_path).unwrap();
    assert!(failed.iter().any(|phase| matches!(&phase.pairing, MobilePairingState::Failed { offer_id, code: protocol::MobileAccessErrorCode::StoreLoadFailed, .. } if *offer_id == cancelled.offer_id)),
        "A failed store replacement must surface StoreLoadFailed");
    assert!(
        !failed
            .iter()
            .any(|phase| matches!(&phase.pairing, MobilePairingState::Cancelled { .. })),
        "Failed durable retirement must not acknowledge cancellation"
    );
    send_host_payload(
        desktop,
        FrameKind::MobilePairingCancel,
        &protocol::MobilePairingCancelPayload {
            offer_id: cancelled.offer_id.clone(),
        },
    )
    .await;
    let phases = mobile_access_events_after_commands(desktop).await;
    assert!(phases.iter().any(|phase| matches!(&phase.pairing, MobilePairingState::Cancelled { offer_id } if *offer_id == cancelled.offer_id)),
        "Cancel must terminate the direct offer over the real protocol");
    assert!(
        redeem(&base, &cancelled_payload).await.status() == 403,
        "Cancelled direct credentials must be unusable over HTTP"
    );

    // A successful retry must retire the credential on disk, not just in memory.
    let bootstrap = fixture.restart_host().await;
    assert!(matches!(
        bootstrap.mobile_access.pairing,
        MobilePairingState::Idle
    ));
    let base = online_addr(&bootstrap.mobile_access.direct_hosting);
    assert!(
        redeem(&base, &cancelled_payload).await.status() == 403,
        "A cancelled offer must remain unusable after a real host restart"
    );
    let desktop = &mut fixture.client;

    scheduled_direct_expiry_allows_renewal().await;

    let offer = start_direct_pairing(desktop).await;
    let payload = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&offer.qr_uri.0))
        .expect("parse the direct pairing QR");
    assert_eq!(
        payload.offer_id, offer.offer_id,
        "the QR must describe the offer the host announced"
    );

    send_host_payload(
        desktop,
        FrameKind::MobilePairingCancel,
        &protocol::MobilePairingCancelPayload {
            offer_id: cancelled.offer_id.clone(),
        },
    )
    .await;
    let phases = mobile_access_events_after_commands(desktop).await;
    assert!(!phases.iter().any(|phase| matches!(&phase.pairing, MobilePairingState::Cancelled { offer_id } if *offer_id == offer.offer_id)),
        "A stale cancellation must not affect the renewed offer");

    let mut mismatched = payload.clone();
    mismatched.offer_id = cancelled.offer_id;
    assert!(
        redeem(&base, &mismatched).await.status() == 403,
        "A valid secret must not redeem another offer ID"
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
        desktop,
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

    let failed_disable = start_direct_pairing(desktop).await;
    let failed_disable_payload =
        DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&failed_disable.qr_uri.0))
            .expect("parse failed-disable direct offer");
    std::fs::rename(&pairings_path, &preserved_path).unwrap();
    std::fs::create_dir(&pairings_path).unwrap();
    desktop
        .replace_setting("/enable_mobile_connections", false, true)
        .await
        .unwrap();
    let states = mobile_access_events_after_commands(desktop).await;
    std::fs::remove_dir(&pairings_path).unwrap();
    std::fs::rename(&preserved_path, &pairings_path).unwrap();
    assert!(
        states.iter().any(|state| matches!(
            state.connection_status,
            protocol::MobileConnectionStatus::Error {
                code: protocol::MobileAccessErrorCode::StoreLoadFailed,
                ..
            }
        )),
        "Master-disable retirement errors must be visible"
    );
    assert!(
        !states
            .iter()
            .any(|state| matches!(state.pairing, MobilePairingState::Idle)),
        "Failed master-disable retirement must not claim Idle"
    );
    send_host_payload(
        desktop,
        FrameKind::MobilePairingCancel,
        &protocol::MobilePairingCancelPayload {
            offer_id: failed_disable.offer_id.clone(),
        },
    )
    .await;
    let states = mobile_access_events_after_commands(desktop).await;
    assert!(
        states.iter().any(|state| matches!(&state.pairing,
        MobilePairingState::Cancelled { offer_id } if *offer_id == failed_disable.offer_id)),
        "A failed master-disable must retain credentials for explicit retirement retry"
    );
    desktop
        .replace_setting("/enable_mobile_connections", true, false)
        .await
        .unwrap();
    let status = wait_for_direct_hosting(
        desktop,
        |state| matches!(state, MobileDirectHostingStatus::Online { .. }),
        "hosting after retirement retry",
    )
    .await;
    assert!(
        redeem(&online_addr(&status), &failed_disable_payload)
            .await
            .status()
            == 403,
        "Explicit retirement retry must invalidate the failed-disable offer"
    );

    let unspent = start_direct_pairing(desktop).await;
    let unspent_payload =
        DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&unspent.qr_uri.0))
            .expect("parse unspent direct offer");
    desktop
        .replace_setting("/enable_mobile_connections", false, true)
        .await
        .unwrap();
    wait_for_direct_hosting(
        desktop,
        |state| matches!(state, MobileDirectHostingStatus::Error { message } if message.contains("mobile connections are off")),
        "master switch reports why direct hosting stopped",
    )
    .await;
    desktop
        .replace_setting("/enable_mobile_connections", true, false)
        .await
        .unwrap();
    let status = wait_for_direct_hosting(
        desktop,
        |state| matches!(state, MobileDirectHostingStatus::Online { .. }),
        "master switch restarts direct hosting",
    )
    .await;
    assert!(
        redeem(&online_addr(&status), &unspent_payload)
            .await
            .status()
            == 403,
        "Disabling mobile access must retire outstanding direct credentials"
    );
}

/// An unpaired or revoked token must not reach the protocol at all.
#[tokio::test]
async fn direct_websocket_refuses_an_unknown_device_token() {
    server::install_default_crypto_provider();
    let bundle = BundleFixture::write();
    let mut fixture = Fixture::new().await;
    let desktop = &mut fixture.client;

    let status = enable_direct_hosting(desktop, &bundle.path()).await;
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

#[derive(serde::Deserialize, serde::Serialize)]
struct ThinServiceSession {
    endpoint: String,
    cookie: String,
    service_commit: String,
}
struct ThinRegistrationFault {
    offer_path: String,
    pairing: std::sync::OnceLock<String>,
    failed: std::sync::atomic::AtomicBool,
}
struct ThinRegistrationHold {
    path: std::sync::OnceLock<String>,
    held: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Notify,
}
struct ThinService {
    session: ThinServiceSession,
    registrations: tokio::sync::mpsc::UnboundedReceiver<String>,
    failure_scope: std::sync::Arc<std::sync::OnceLock<ThinRegistrationFault>>,
    failed_registrations: tokio::sync::mpsc::UnboundedReceiver<()>,
    held_registration: std::sync::Arc<std::sync::OnceLock<ThinRegistrationHold>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ThinService {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl ThinService {
    async fn start(file: &std::path::Path) -> Self {
        use axum::{
            Router,
            body::{Body, to_bytes},
            extract::{Request, State},
            response::Response,
        };
        #[derive(Clone)]
        struct Proxy {
            origin: String,
            http: reqwest::Client,
            completed: tokio::sync::mpsc::UnboundedSender<String>,
            failure_scope: std::sync::Arc<std::sync::OnceLock<ThinRegistrationFault>>,
            failed_registrations: tokio::sync::mpsc::UnboundedSender<()>,
            held_registration: std::sync::Arc<std::sync::OnceLock<ThinRegistrationHold>>,
        }
        async fn forward(State(proxy): State<Proxy>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, 1024 * 1024)
                .await
                .expect("bounded private Service request");
            let path = parts.uri.path_and_query().expect("Service path").as_str();
            let registration = parts.method == axum::http::Method::POST
                && parts.uri.path().ends_with("/bootstrap");
            if registration
                && let Some(hold) = proxy.held_registration.get()
                && hold.path.get().is_some_and(|path| parts.uri.path() == path)
                && !hold.held.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                hold.arrived.send(()).expect("owned registration arrival");
                hold.release.notified().await;
            }
            if registration
                && let Some(fault) = proxy.failure_scope.get()
                && fault.pairing.get().is_some_and(|pairing| {
                    parts.uri.path() == format!("/api/tyde/mobile/v1/pairings/{pairing}/bootstrap")
                })
                && !fault.failed.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let _ = proxy.failed_registrations.send(());
                return Response::builder().status(503).body(Body::empty()).unwrap();
            }
            let mut headers = parts.headers;
            headers.remove(axum::http::header::HOST);
            let response = proxy
                .http
                .request(parts.method, format!("{}{path}", proxy.origin))
                .headers(headers)
                .body(body)
                .send()
                .await
                .unwrap_or_else(|_| panic!("actual local Service command failed"));
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response
                .bytes()
                .await
                .unwrap_or_else(|_| panic!("actual Service response did not complete"));
            if status.is_success()
                && let Some(fault) = proxy.failure_scope.get()
                && parts.uri.path() == fault.offer_path
            {
                let response: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("actual offer poll JSON");
                if let Some(pairing) = response["pairing_id"].as_str() {
                    if let Some(previous) = fault.pairing.get() {
                        assert!(previous == pairing, "actual offer binding must not change");
                    } else {
                        assert!(
                            fault.pairing.set(pairing.to_owned()).is_ok(),
                            "one authoritative offer binding"
                        );
                    }
                }
            }
            if status.is_success()
                && parts.uri.path() == "/api/tyde/mobile/v1/host/offers"
                && let Some(hold) = proxy.held_registration.get()
            {
                let created: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("actual Service offer response");
                let offer = created["offer_id"]
                    .as_str()
                    .expect("actual created offer identity");
                if hold.path.get().is_none() {
                    assert!(
                        hold.path
                            .set(format!("/api/tyde/mobile/v1/host/offers/{offer}/bootstrap"))
                            .is_ok(),
                        "one actual fresh offer binding"
                    );
                }
            }
            if registration && status.is_success() {
                let _ = proxy.completed.send(parts.uri.path().to_owned());
            }
            let mut result = Response::new(Body::from(bytes));
            *result.status_mut() = status;
            *result.headers_mut() = headers;
            result
        }
        let mut session: ThinServiceSession = serde_json::from_slice(
            &tokio::fs::read(file)
                .await
                .expect("private routed Service fixture file"),
        )
        .expect("routed Service fixture schema");
        assert!(
            session.service_commit == "c9d701823b2fb6bfefb1adc0e3fb4f8dee7c2fdd",
            "Service fixture source must match routed source"
        );
        let url = url::Url::parse(&session.endpoint).expect("routed endpoint URL");
        let host: std::net::IpAddr = url
            .host_str()
            .expect("local fixture host")
            .parse()
            .expect("literal loopback fixture");
        assert!(
            url.scheme() == "http" && host.is_loopback(),
            "integration is scoped to the routed local Service fixture"
        );
        let (completed, registrations) = tokio::sync::mpsc::unbounded_channel();
        let (failed_tx, failed_registrations) = tokio::sync::mpsc::unbounded_channel();
        let failure_scope = std::sync::Arc::new(std::sync::OnceLock::new());
        let held_registration = std::sync::Arc::new(std::sync::OnceLock::new());
        let proxy = Proxy {
            origin: url.origin().ascii_serialization(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            completed,
            failure_scope: failure_scope.clone(),
            failed_registrations: failed_tx,
            held_registration: held_registration.clone(),
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        session.endpoint = format!(
            "http://{}/api/tyde/mobile/v1",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(forward).with_state(proxy))
                .await
                .expect("owned transparent Service transport");
        });
        Self {
            session,
            registrations,
            failure_scope,
            failed_registrations,
            held_registration,
            task,
        }
    }
    fn hold_next_offer_registration(&self) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        let (arrived, receiver) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            self.held_registration
                .set(ThinRegistrationHold {
                    path: std::sync::OnceLock::new(),
                    held: std::sync::atomic::AtomicBool::new(false),
                    arrived,
                    release: tokio::sync::Notify::new(),
                })
                .is_ok(),
            "one explicit registration hold"
        );
        receiver
    }
    fn fail_pairing_registration(&self, offer: &protocol::MobilePairingOfferId) {
        assert!(
            self.failure_scope
                .set(ThinRegistrationFault {
                    offer_path: format!("/api/tyde/mobile/v1/host/offers/{}", offer.as_str()),
                    pairing: std::sync::OnceLock::new(),
                    failed: std::sync::atomic::AtomicBool::new(false),
                })
                .is_ok(),
            "one resource-scoped registration failure"
        );
    }
    async fn registered(&mut self, pairing: &str) {
        self.registered_path(format!("/api/tyde/mobile/v1/pairings/{pairing}/bootstrap"))
            .await;
    }
    async fn registered_path(&mut self, expected: String) {
        while let Some(path) = self.registrations.recv().await {
            if path == expected {
                return;
            }
        }
        panic!("actual host registration transport ended before receipt");
    }
}

async fn thin_managed_offer(fixture: &mut Fixture) -> protocol::MobilePairingQrUri {
    send_host_payload(
        &mut fixture.client,
        FrameKind::MobilePairingStart,
        &MobilePairingStartPayload { direct: false },
    )
    .await;
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "actual managed offer registered",
        |event| {
            assert!(
                event.kind != FrameKind::CommandError,
                "real host rejected managed offer creation"
            );
            if event.kind == FrameKind::MobileAccessState {
                assert!(
                    !matches!(
                        event
                            .parse_payload::<MobileAccessStatePayload>()
                            .unwrap()
                            .pairing,
                        MobilePairingState::Failed { .. }
                    ),
                    "real host failed managed offer registration"
                );
            }
            event.kind == FrameKind::MobilePairingOffer
        },
    )
    .await
    .parse_payload::<MobilePairingOfferPayload>()
    .unwrap()
    .qr_uri
}

// The first native ABI is retained while current full clients are built from
// actual source. A configured older executable additionally proves process,
// store, wire and asset handover; it is never emulated by editing versions.
#[derive(serde::Serialize, serde::Deserialize)]
struct ThinLaunch {
    control: std::net::SocketAddr,
    challenge: String,
    store: std::path::PathBuf,
    bundle: std::path::PathBuf,
    origin: String,
    service: Option<String>,
    publisher: ThinPublisher,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ThinPublisher {
    proxy: String,
    certificate: String,
}
impl ThinPublisher {
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::HeaderName::from_static("x-tyde-native-publication-sim"),
                reqwest::header::HeaderValue::from_static("1"),
            )]))
            .proxy(reqwest::Proxy::https(&self.proxy).expect("owned publisher transport"))
            .add_root_certificate(
                reqwest::Certificate::from_pem(self.certificate.as_bytes())
                    .expect("owned publisher CA"),
            )
            .build()
            .expect("owned HTTPS publisher client")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ThinControl {
    Ready {
        challenge: String,
        address: String,
        protocol: u32,
        release: String,
    },
    Offer {
        qr_uri: protocol::MobilePairingQrUri,
    },
    DirectOffer,
    ManagedOffer,
    Stop {
        expected_devices: usize,
    },
    Stopped {
        session: protocol::SessionId,
        devices: Vec<protocol::MobileDeviceId>,
    },
}

struct ThinChannel {
    read: tokio::io::Lines<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>,
    write: tokio::net::tcp::OwnedWriteHalf,
}
impl ThinChannel {
    fn new(socket: tokio::net::TcpStream) -> Self {
        use tokio::io::AsyncBufReadExt;
        let (read, write) = socket.into_split();
        Self {
            read: tokio::io::BufReader::new(read).lines(),
            write,
        }
    }
    async fn send(&mut self, value: &ThinControl) {
        use tokio::io::AsyncWriteExt;
        self.write
            .write_all(format!("{}\n", serde_json::to_string(value).unwrap()).as_bytes())
            .await
            .expect("private host control write");
    }
    async fn receive(&mut self) -> ThinControl {
        let line = self
            .read
            .next_line()
            .await
            .expect("private host control read")
            .expect("owned host exited before its event");
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("invalid private host event"))
    }
}

#[derive(serde::Deserialize)]
struct ThinBaselineCustody {
    source_commit: String,
    protocol_version: u32,
    executable: std::path::PathBuf,
    executable_sha256: String,
    client_archive: std::path::PathBuf,
    client_archive_sha256: String,
}
struct ThinSource {
    custody: Option<ThinBaselineCustody>,
    bundle: Option<tempfile::TempDir>,
    executable: std::path::PathBuf,
    directory: std::path::PathBuf,
    expected_protocol: u32,
}
impl ThinSource {
    async fn load(record: Option<&std::path::Path>, current: &std::path::Path) -> Self {
        use sha2::Digest;
        let (custody, bundle, executable, directory, expected_protocol) = match record {
            Some(record) => {
                let custody: ThinBaselineCustody = serde_json::from_slice(
                    &tokio::fs::read(record)
                        .await
                        .expect("baseline custody file"),
                )
                .expect("baseline custody schema");
                for (file, expected) in [
                    (&custody.executable, &custody.executable_sha256),
                    (&custody.client_archive, &custody.client_archive_sha256),
                ] {
                    let bytes = tokio::fs::read(file)
                        .await
                        .expect("retained source artifact");
                    assert!(
                        format!("{:x}", sha2::Sha256::digest(bytes)) == *expected,
                        "retained source artifact differs from custody"
                    );
                }
                assert!(
                    custody.protocol_version != protocol::PROTOCOL_VERSION,
                    "cross-source proof requires genuinely different wire revisions"
                );
                let bundle = tempfile::tempdir().unwrap();
                assert!(
                    tokio::process::Command::new("tar")
                        .arg("-xzf")
                        .arg(&custody.client_archive)
                        .arg("-C")
                        .arg(bundle.path())
                        .status()
                        .await
                        .unwrap()
                        .success()
                );
                let directory = bundle.path().to_owned();
                let executable = custody.executable.clone();
                let expected = custody.protocol_version;
                (Some(custody), Some(bundle), executable, directory, expected)
            }
            None => (
                None,
                None,
                std::env::current_exe().expect("current checked test executable"),
                current.to_owned(),
                protocol::PROTOCOL_VERSION,
            ),
        };
        Self {
            custody,
            bundle,
            executable,
            directory,
            expected_protocol,
        }
    }
}
struct ThinBaseline {
    child: tokio::process::Child,
    control: ThinChannel,
    address: String,
    release: String,
}
impl ThinBaseline {
    async fn start(
        source: &ThinSource,
        store: &std::path::Path,
        origin: &str,
        service: Option<&str>,
        publisher: &ThinPublisher,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let challenge = uuid::Uuid::new_v4().to_string();
        let launch = ThinLaunch {
            control: listener.local_addr().unwrap(),
            challenge: challenge.clone(),
            store: store.to_owned(),
            bundle: source.directory.clone(),
            origin: origin.into(),
            service: service.map(str::to_owned),
            publisher: publisher.clone(),
        };
        let mut child = tokio::process::Command::new(&source.executable)
            .args([
                "--exact",
                "native_shell_loads_exact_clients_with_retained_pairing",
                "--nocapture",
            ])
            .env(
                "TYDE_THIN_BASELINE_CONTROL",
                serde_json::to_string(&launch).unwrap(),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("owned actual server process");
        let (socket, _) = tokio::select! {socket=listener.accept()=>socket.expect("private process control"),status=child.wait()=>panic!("source process exited before readiness; success={}",status.expect("source exit event").success())};
        let mut control = ThinChannel::new(socket);
        let ThinControl::Ready {
            challenge: received,
            address,
            protocol: wire,
            release,
        } = control.receive().await
        else {
            panic!("expected actual source readiness")
        };
        assert!(
            received == challenge && wire == source.expected_protocol,
            "source readiness must belong to its checked executable"
        );
        Self {
            child,
            control,
            address,
            release,
        }
    }
    async fn offer(&mut self) -> protocol::MobilePairingQrUri {
        self.control.send(&ThinControl::DirectOffer).await;
        let ThinControl::Offer { qr_uri } = self.control.receive().await else {
            panic!("expected actual source offer")
        };
        qr_uri
    }
    async fn managed_offer(&mut self) -> protocol::MobilePairingQrUri {
        self.control.send(&ThinControl::ManagedOffer).await;
        let ThinControl::Offer { qr_uri } = self.control.receive().await else {
            panic!("expected actual managed offer")
        };
        qr_uri
    }
    async fn stop(
        &mut self,
        expected_devices: usize,
    ) -> (protocol::SessionId, Vec<protocol::MobileDeviceId>) {
        self.control
            .send(&ThinControl::Stop { expected_devices })
            .await;
        let ThinControl::Stopped { session, devices } = self.control.receive().await else {
            panic!("expected actual source checkpoint")
        };
        assert!(
            self.child.wait().await.unwrap().success(),
            "source process must exit before store takeover"
        );
        (session, devices)
    }
}

fn thin_release() -> String {
    serde_json::from_str::<serde_json::Value>(include_str!("../../package.json")).unwrap()["version"].as_str().unwrap().to_owned()
}
async fn thin_fixture(
    store: std::path::PathBuf,
    bundle: &std::path::Path,
    origin: &str,
    service: Option<&str>,
    publisher: &ThinPublisher,
) -> (Fixture, fixture::TestAgent) {
    use server::backend::mock::{MockScript, MockTurn};
    eprintln!("NATIVE_BOUNDARY initializing real host");
    let mut fixture = Fixture::new_at_existing_store_with_runtime_config(
        store,
        server::HostRuntimeConfig {
            mobile_managed_service_base_url: service.map(str::to_owned),
            mobile_bootstrap_http: Some(publisher.client()),
            ..Default::default()
        },
    )
    .await;
    eprintln!("NATIVE_BOUNDARY spawning protocol fixture agent");
    let agent = fixture
        .spawn_scripted(
            "Retained compatibility chat",
            MockScript::one(MockTurn::text("Compatibility response"))
                .then(MockTurn::text("Compatibility follow-up response")),
        )
        .await;
    eprintln!("NATIVE_BOUNDARY awaiting provider completion event");
    fixture
        .next_chat_event_matching(&agent, "settled native conversation", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;
    if fixture.bootstrap.settings.mobile_direct_hosting_enabled {
        eprintln!("NATIVE_BOUNDARY validating unchanged restored host settings");
        let settings = &fixture.bootstrap.settings;
        assert!(
            settings.enable_mobile_connections
                && settings.mobile_direct_bundle_dir.as_deref() == bundle.to_str()
                && settings.mobile_direct_public_origin.as_deref() == Some(origin),
            "owned process restart must preserve the actual configured host"
        );
        assert!(
            matches!(
                fixture.bootstrap.mobile_access.direct_hosting,
                MobileDirectHostingStatus::Online { .. }
            ),
            "restored direct hosting must be ready, not reconfigured with cold-store CAS values"
        );
    } else {
        eprintln!("NATIVE_BOUNDARY enabling real direct hosting in the fresh store");
        enable_direct_hosting(&mut fixture.client, bundle.to_str().unwrap()).await;
        fixture
            .client
            .replace_setting(
                "/mobile_direct_public_origin",
                Some(origin),
                Some("https://tyde.test.internal"),
            )
            .await
            .unwrap();
        fixture::next_frame_matching_on(&mut fixture.client, "native origin committed", |event| {
            event.kind == FrameKind::HostSettings
                && event
                    .parse_payload::<settings_model::HostSettingsPayload>()
                    .is_ok_and(|p| {
                        p.settings.mobile_direct_public_origin.as_deref() == Some(origin)
                    })
        })
        .await;
    }
    eprintln!("NATIVE_BOUNDARY real host ready");
    (fixture, agent)
}
async fn thin_identity(
    fixture: &Fixture,
    agent: &fixture::TestAgent,
    expected_devices: usize,
) -> (protocol::SessionId, Vec<protocol::MobileDeviceId>) {
    eprintln!("NATIVE_BOUNDARY awaiting paired identity bootstrap");
    let (_, state) = fixture.connect_with_bootstrap().await;
    eprintln!("NATIVE_BOUNDARY paired identity bootstrap complete");
    let session = state
        .agents
        .iter()
        .find(|current| current.agent_id == agent.new_agent.agent_id)
        .and_then(|current| current.session_id.clone());
    tokio::fs::write(fixture.store_dir().join("native-checkpoint-state.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"provider_session_present":session.is_some(),
            "expected_devices":expected_devices,"actual_devices":state.mobile_access.paired_devices.len(),
            "managed_mobile_enabled":state.settings.enable_mobile_connections,
            "pairing_active":matches!(state.mobile_access.pairing,MobilePairingState::Active{..})})).unwrap()).await.unwrap();
    let session = session.expect("actual initialized provider session");
    let devices = state
        .mobile_access
        .paired_devices
        .iter()
        .map(|device| device.device_id.clone())
        .collect::<Vec<_>>();
    assert!(
        devices.len() == expected_devices,
        "native client must pair once before source handover"
    );
    (session, devices)
}
async fn thin_child(config: ThinLaunch) {
    let mut control = ThinChannel::new(
        tokio::net::TcpStream::connect(config.control)
            .await
            .unwrap(),
    );
    let checkpoint = config.store.join("mock-provider-checkpoint.json");
    if tokio::fs::try_exists(&checkpoint).await.unwrap() {
        server::backend::mock::restore_sessions_for_restart(&checkpoint)
            .expect("actual owned-process provider checkpoint");
        // The next checkpoint is create-new, not an overwrite of its donor.
        tokio::fs::rename(
            &checkpoint,
            config.store.join(format!(
                "consumed-provider-checkpoint-{}.json",
                uuid::Uuid::new_v4()
            )),
        )
        .await
        .expect("preserve consumed actual provider checkpoint");
    }
    let (mut fixture, agent) = thin_fixture(
        config.store,
        &config.bundle,
        &config.origin,
        config.service.as_deref(),
        &config.publisher,
    )
    .await;
    let (_, state) = fixture.connect_with_bootstrap().await;
    control
        .send(&ThinControl::Ready {
            challenge: config.challenge,
            address: online_addr(&state.mobile_access.direct_hosting),
            protocol: protocol::PROTOCOL_VERSION,
            release: thin_release(),
        })
        .await;
    let expected_devices = loop {
        match control.receive().await {
            ThinControl::DirectOffer => {
                control
                    .send(&ThinControl::Offer {
                        qr_uri: start_direct_pairing(&mut fixture.client).await.qr_uri,
                    })
                    .await;
            }
            ThinControl::ManagedOffer => {
                control
                    .send(&ThinControl::Offer {
                        qr_uri: thin_managed_offer(&mut fixture).await,
                    })
                    .await
            }
            ThinControl::Stop { expected_devices } => break expected_devices,
            _ => panic!("unexpected private source-control command"),
        }
    };
    let (session, devices) = thin_identity(&fixture, &agent, expected_devices).await;
    fixture
        .host_for_test()
        .shutdown_for_restart()
        .await
        .expect("actual host checkpoint");
    server::backend::mock::save_sessions_for_restart(
        &fixture.store_dir().join("mock-provider-checkpoint.json"),
    )
    .expect("actual private provider checkpoint");
    control
        .send(&ThinControl::Stopped { session, devices })
        .await;
    // This is an explicitly owned one-case child process. Exit after the
    // durable checkpoint, before dropping its desktop subscriber cancels a QR.
    std::process::exit(0);
}

#[tokio::test]
async fn native_shell_loads_exact_clients_with_retained_pairing() {
    use server::backend::mock::MockTurn;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    server::install_default_crypto_provider();
    server::set_host_release_version(&thin_release());
    if let Some(config) = std::env::var_os("TYDE_THIN_BASELINE_CONTROL") {
        let config = serde_json::from_str(&config.to_string_lossy())
            .unwrap_or_else(|_| panic!("invalid private source configuration"));
        thin_child(config).await;
        return;
    }
    let current = std::path::PathBuf::from(
        std::env::var_os("TYDE_NATIVE_CURRENT_CLIENT")
            .expect("canonical check must build the actual current client"),
    );
    let mut service = match std::env::var_os("TYDE_THIN_NATIVE_SERVICE") {
        Some(file) => Some(ThinService::start(std::path::Path::new(&file)).await),
        None => None,
    };
    let store = tempfile::tempdir().unwrap();
    let baseline_record =
        std::env::var_os("TYDE_THIN_NATIVE_BASELINE").map(std::path::PathBuf::from);
    let source = ThinSource::load(baseline_record.as_deref(), &current).await;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let evidence_root = root.join("target/thin-native-proof");
    tokio::fs::create_dir_all(&evidence_root).await.unwrap();
    let evidence = tempfile::tempdir_in(&evidence_root).unwrap().keep();
    let mut child = tokio::process::Command::new("node")
        .arg("tools/native-client-upgrade.mjs")
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("actual frozen shell driver");
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    async fn send(input: &mut tokio::process::ChildStdin, value: serde_json::Value) {
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn event(
        output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
        expected: &str,
    ) -> serde_json::Value {
        let line = output
            .next_line()
            .await
            .expect("private driver event read")
            .expect("native driver exited before event");
        let value: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("invalid private driver event"));
        assert!(
            value["event"].as_str() == Some(expected),
            "driver reached an unexpected boundary"
        );
        value
    }
    send(&mut input,serde_json::json!({"current_bundle":current,"baseline_bundle":source.bundle.as_ref().map(|b|b.path()),"evidence":evidence,"service":service.as_ref().map(|s|&s.session)})).await;
    let ready = event(&mut output, "ready").await;
    let publisher: ThinPublisher =
        serde_json::from_value(ready["publisher"].clone()).expect("real publisher transport");
    let mut baseline = ThinBaseline::start(
        &source,
        store.path(),
        "https://native-host.test",
        service.as_ref().map(|s| s.session.endpoint.as_str()),
        &publisher,
    )
    .await;
    event(&mut output, "publisher-held").await;
    let independent = reqwest::Client::new();
    assert!(
        independent
            .get(format!("http://{}/tyde/", baseline.address))
            .send()
            .await
            .expect("PWA while publisher held")
            .status()
            .is_success(),
        "PWA hosting must not await native publication"
    );
    let authority = independent
        .post(format!("http://{}/tyde/bootstrap", baseline.address))
        .json(&protocol::MobileDirectBootstrapRequest {
            native_bridge_abi: 6,
            offer_id: None,
            offer_secret: None,
        })
        .send()
        .await
        .expect("actor while publisher held");
    assert!(
        authority.status() == 400,
        "mobile actor must reject missing authority while publication is held"
    );
    // Existing direct/PWA pairing must issue its real QR while publication is held.
    let qr_uri = baseline.offer().await;
    send(&mut input, serde_json::json!({"event":"release-publisher"})).await;
    event(&mut output, "publisher-released").await;
    let actual_offer = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&qr_uri.0))
        .expect("actual direct offer");
    let incompatible = reqwest::Client::new()
        .post(format!("http://{}/tyde/bootstrap", baseline.address))
        .json(&protocol::MobileDirectBootstrapRequest {
            native_bridge_abi: 0,
            offer_id: Some(actual_offer.offer_id),
            offer_secret: Some(actual_offer.offer_secret),
        })
        .send()
        .await
        .unwrap_or_else(|_| panic!("real incompatible ABI request failed"));
    assert!(
        incompatible.status() == 403 && header(&incompatible, "cache-control") == "no-store",
        "incompatible ABI must be refused before execution"
    );
    assert!(
        incompatible
            .json::<protocol::MobileDirectErrorResponse>()
            .await
            .expect("typed refusal")
            .code
            == protocol::MobileAccessErrorCode::NativeAbiIncompatible,
        "refusal must distinguish app ABI from host baseline"
    );
    send(
        &mut input,
        serde_json::json!({"qr_uri":qr_uri,"release":baseline.release,"address":baseline.address}),
    )
    .await;
    event(&mut output, "paired").await;
    let managed_pairing = if let Some(service) = &mut service {
        let qr_uri = baseline.managed_offer().await;
        send(&mut input,serde_json::json!({"event":"managed-offer","qr_uri":qr_uri,"release":baseline.release,"phase":"before"})).await;
        let saved = event(&mut output, "managed-saved").await;
        let pairing = saved["pairing_id"]
            .as_str()
            .expect("private pairing binding")
            .to_owned();
        service.registered(&pairing).await;
        send(
            &mut input,
            serde_json::json!({"event":"managed-registered"}),
        )
        .await;
        event(&mut output, "managed-verified").await;
        Some(pairing)
    } else {
        None
    };
    eprintln!("NATIVE_BOUNDARY checkpointing and terminating the actual source process");
    let (session, devices) = baseline.stop(if service.is_some() { 2 } else { 1 }).await;
    eprintln!("NATIVE_BOUNDARY source process exited; awaiting actual unavailable discovery");
    send(&mut input, serde_json::json!({"event":"host-stopped"})).await;
    event(&mut output, "offline").await;
    server::backend::mock::restore_sessions_for_restart(
        &store.path().join("mock-provider-checkpoint.json"),
    )
    .expect("restore actual provider records");
    send(
        &mut input,
        serde_json::json!({"event":"restart-publication"}),
    )
    .await;
    event(&mut output, "publication-fault-armed").await;
    let mut fixture = Fixture::new_at_existing_store_with_runtime_config(
        store.path().to_owned(),
        server::HostRuntimeConfig {
            mobile_managed_service_base_url: service.as_ref().map(|s| s.session.endpoint.clone()),
            mobile_bootstrap_http: Some(publisher.client()),
            ..Default::default()
        },
    )
    .await;
    event(&mut output, "publication-host-failed").await;
    if source.custody.is_some() {
        let previous = fixture.bootstrap.settings.mobile_direct_bundle_dir.clone();
        let write = fixture
            .client
            .replace_setting(
                "/mobile_direct_bundle_dir",
                Some(current.to_str().unwrap()),
                previous,
            )
            .await
            .unwrap();
        fixture::expect_settings_write_applied(
            &mut fixture.client,
            &write,
            "current source bundle configuration committed",
        )
        .await;
    }
    let (_, state) = fixture.connect_with_bootstrap().await;
    assert!(
        state
            .mobile_access
            .paired_devices
            .iter()
            .map(|device| device.device_id.clone())
            .collect::<Vec<_>>()
            == devices,
        "source takeover must retain real paired identities"
    );
    let restored = fixture
        .restored_agents(&state, std::slice::from_ref(&session))
        .await;
    let agent = restored.get(&session).expect("retained provider restored");
    fixture
        .mock_by_id(&agent.agent_id)
        .await
        .enqueue(MockTurn::text("Compatibility post-upgrade response"))
        .await;
    let title = "Retained chat after source upgrade";
    fixture
        .client
        .set_agent_name(&agent.instance_stream, title.into())
        .await
        .unwrap();
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "replacement title committed",
        |event| {
            event.kind == FrameKind::AgentRenamed
                && event
                    .parse_payload::<protocol::AgentRenamedPayload>()
                    .is_ok_and(|p| p.agent_id == agent.agent_id && p.name == title)
        },
    )
    .await;
    if let (Some(service), Some(pairing)) = (&mut service, &managed_pairing) {
        service.registered(pairing).await;
    }
    let (_, state) = fixture.connect_with_bootstrap().await;
    send(&mut input,serde_json::json!({"address":online_addr(&state.mobile_access.direct_hosting),"release":thin_release(),"title":title})).await;
    let upgraded = event(&mut output, "upgraded").await;
    tokio::fs::write(evidence.join("publication-ownership-recovery-proof.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "canonical_publisher_https": true, "held_response": true, "independent_pwa_http_success": true,
        "independent_actor_authority_rejection": true, "replacement_publisher_503_observed": true,
        "recovered_publication": true, "same_retained_store": true, "fresh_reply_after_recovery": true,
        "limit": "Owned HTTPS publisher and real local hosts. No production CDN/TLS or RTC claim."
    })).unwrap()).await.unwrap();
    if source.custody.is_some() {
        assert!(
            upgraded["documents"].as_u64() == Some(2),
            "different clients require one deliberate client lifecycle change"
        );
    }
    if let Some(service) = &mut service {
        let current_source = ThinSource::load(None, &current).await;
        let lifecycle_store = tempfile::tempdir_in(&evidence).unwrap().keep();
        eprintln!("NATIVE_BOUNDARY starting owned current managed-lifecycle witness");
        let mut witness = ThinBaseline::start(
            &current_source,
            &lifecycle_store,
            "https://lifecycle-host.test",
            Some(&service.session.endpoint),
            &publisher,
        )
        .await;
        let seed = witness.offer().await;
        let seed = DirectMobilePairingQrPayload::from_uri(pairing_uri_fragment(&seed.0)).unwrap();
        let admission = reqwest::Client::new()
            .post(format!("http://{}/tyde/bootstrap", witness.address))
            .json(&protocol::MobileDirectBootstrapRequest {
                native_bridge_abi: 6,
                offer_id: Some(seed.offer_id),
                offer_secret: Some(seed.offer_secret),
            })
            .send()
            .await
            .expect("current witness publication ready");
        assert!(
            admission.status().is_success(),
            "witness must publish its actual client before making a registered offer"
        );
        let mut held = service.hold_next_offer_registration();
        witness.control.send(&ThinControl::ManagedOffer).await;
        held.recv()
            .await
            .expect("fresh managed-offer bootstrap registration held");
        let authority = reqwest::Client::new()
            .post(format!("http://{}/tyde/bootstrap", witness.address))
            .bearer_auth("invalid-disposable-device")
            .json(&protocol::MobileDirectBootstrapRequest {
                native_bridge_abi: 6,
                offer_id: None,
                offer_secret: None,
            })
            .send()
            .await
            .expect("independent command during fresh offer registration");
        assert!(
            authority.status() == reqwest::StatusCode::FORBIDDEN,
            "fresh managed-offer registration must not block independent mobile actor authority"
        );
        service
            .held_registration
            .get()
            .unwrap()
            .release
            .notify_one();
        // ManagedOffer is already in flight; consume its completion without
        // issuing the separate DirectOffer command from witness.offer().
        let ThinControl::Offer { qr_uri } = witness.control.receive().await else {
            panic!("expected the held managed offer completion")
        };
        let mut pending = Some(qr_uri);
        tokio::fs::write(evidence.join("held-service-registration-proof.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"real_service_registration_held":true,
                "fresh_offer_creation_after_known_publication":true,"offer_binding_from_actual_create_response":true,
                "direct_actor_authority_settled_before_release":true,"http_status":403,
                "offer_delivered_after_registration":true,
                "synchronization":"actual registration arrival and explicit release; no sleeps/time thresholds"})).unwrap()).await.unwrap();
        eprintln!("NATIVE_BOUNDARY initial managed offer is pending; checkpointing witness");
        witness.stop(0).await;
        eprintln!("NATIVE_BOUNDARY managed witness process exited with its real pending offer");
        for (phase, expected_devices) in [("after", 1), ("held", 2), ("failed_registration", 3)] {
            send(
                &mut input,
                serde_json::json!({"event":"arm-managed-publication","phase":phase}),
            )
            .await;
            event(&mut output, "managed-publication-armed").await;
            witness = ThinBaseline::start(
                &current_source,
                &lifecycle_store,
                "https://lifecycle-host.test",
                Some(&service.session.endpoint),
                &publisher,
            )
            .await;
            event(&mut output, "managed-publication-held").await;
            eprintln!("NATIVE_BOUNDARY managed publication held phase={phase}");
            let qr_uri = match pending.take() {
                Some(pending) => pending,
                None => witness.managed_offer().await,
            };
            let offer = mobile_pairing::ManagedMobilePairingQrPayload::from_uri(
                pairing_uri_fragment(&qr_uri.0),
            )
            .expect("actual managed lifecycle offer");
            // The old publication completed before the prior process exited.
            // Only a new completed registration after this release can admit the probe.
            while service.registrations.try_recv().is_ok() {}
            send(&mut input,serde_json::json!({"event":"resume-managed-publication","address":witness.address,"qr_uri":qr_uri,"release":thin_release(),"phase":phase})).await;
            event(&mut output, "managed-publication-released").await;
            service
                .registered_path(format!(
                    "/api/tyde/mobile/v1/host/offers/{}/bootstrap",
                    offer.offer_id.as_str()
                ))
                .await;
            eprintln!("NATIVE_BOUNDARY current managed offer registration completed phase={phase}");
            if phase == "failed_registration" {
                service.fail_pairing_registration(&offer.offer_id);
            }
            send(
                &mut input,
                serde_json::json!({"event":"managed-offer-registered"}),
            )
            .await;
            let saved = event(&mut output, "managed-saved").await;
            let outcome = if phase == "failed_registration" {
                service
                    .failed_registrations
                    .recv()
                    .await
                    .expect("actual scoped paired registration returned503");
                "managed-registration-failed"
            } else {
                service
                    .registered(
                        saved["pairing_id"]
                            .as_str()
                            .expect("private current binding"),
                    )
                    .await;
                "managed-registered"
            };
            send(&mut input, serde_json::json!({"event":outcome})).await;
            event(&mut output, "managed-verified").await;
            witness.stop(expected_devices).await;
        }
        tokio::fs::write(evidence.join("managed-publication-lifecycle-proof.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "real_host_process_exits":4,"unchanged_pending_offer_resumed":true,"offer_created_while_publication_held":true,
            "same_installed_shell":true,"canonical_pairing_completions":2,
            "limit":"Real local Service and production mobile actor; no RTC or native platform claim."
        })).unwrap()).await.unwrap();
    }
    let second_store = tempfile::tempdir().unwrap();
    let mut second = ThinBaseline::start(
        &source,
        second_store.path(),
        "https://second-host.test",
        None,
        &publisher,
    )
    .await;
    let qr_uri = second.offer().await;
    send(
        &mut input,
        serde_json::json!({"address":second.address,"qr_uri":qr_uri,"release":second.release}),
    )
    .await;
    event(&mut output, "need-third-offer").await;
    send(
        &mut input,
        serde_json::json!({"qr_uri":second.offer().await}),
    )
    .await;
    let switched = event(&mut output, "switched").await;
    assert!(
        switched["pair_requests"].as_u64() == Some(3),
        "host switching cannot re-pair an existing identity"
    );
    let device_id = protocol::MobileDeviceId(
        switched["device_id"]
            .as_str()
            .expect("private retained device binding")
            .to_owned(),
    );
    fixture::send_host_payload(
        &mut fixture.client,
        FrameKind::MobileDeviceRevoke,
        &protocol::MobileDeviceRevokePayload {
            device_id: device_id.clone(),
        },
    )
    .await;
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "real owner revocation committed",
        |event| {
            event.kind == FrameKind::MobileAccessState
                && event
                    .parse_payload::<MobileAccessStatePayload>()
                    .is_ok_and(|state| {
                        state
                            .paired_devices
                            .iter()
                            .all(|device| device.device_id != device_id)
                    })
        },
    )
    .await;
    send(&mut input, serde_json::json!({"event":"revoked"})).await;
    event(&mut output, "revocation-verified").await;
    send(&mut input, serde_json::json!({"event":"finish"})).await;
    event(&mut output, "done").await;
    drop(input);
    assert!(
        child.wait().await.unwrap().success(),
        "actual frozen shell flow failed"
    );
    second.stop(2).await;
    if let Some(custody) = &source.custody {
        let current_source = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        let current_source = String::from_utf8(current_source.stdout)
            .unwrap()
            .trim()
            .to_owned();
        assert!(
            current_source != custody.source_commit,
            "cross-source proof requires distinct commits"
        );
        tokio::fs::write(evidence.join("source-upgrade-proof.json"),serde_json::to_vec_pretty(&serde_json::json!({"baseline_source":custody.source_commit,"recipient_source":current_source,"baseline_protocol":custody.protocol_version,"recipient_protocol":protocol::PROTOCOL_VERSION,"baseline_client_archive_sha256":custody.client_archive_sha256,"same_store":true,"actual_provider_records":true,"retained_devices":devices.len(),"pair_requests":switched["pair_requests"],"documents":switched["documents"]})).unwrap()).await.unwrap();
    }
    eprintln!(
        "Native shell/client process boundaries passed; proof directory={}",
        evidence.display()
    );
}
