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

use protocol::{
    CommandErrorPayload, Envelope, FrameKind, MobileAccessStatePayload, MobileDirectHostingStatus,
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
        let store_dir = tempfile::tempdir().expect("create direct hosting store dir");
        let host = server::spawn_host_with_mock_backend(
            store_dir.path().join("sessions.json"),
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
