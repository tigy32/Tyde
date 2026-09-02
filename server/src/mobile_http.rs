//! Direct HTTP hosting of the mobile web app.
//!
//! The managed path serves the loader shell and the immutable app bundles from
//! `tycode.dev` and tunnels the client's protocol connection over MQTT. This
//! module serves the same URL space from the host itself, for networks that
//! would rather reach an internal site than have a client tunnel out to a
//! broker.
//!
//! Tyde speaks plain HTTP here and expects the deployment to terminate TLS in
//! front of it. That is not a shortcut for later: browsers withhold service
//! workers, WebCrypto (which the loader's SRI verification needs), camera
//! access and push from any origin that is not a secure context, so a
//! bare-HTTP origin is a degraded app no matter who serves it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use tokio_util::sync::CancellationToken;

/// Loopback by default: the expected deployment puts a TLS-terminating proxy
/// in front, and a proxy on this machine can reach loopback. Widening the bind
/// is an explicit choice because it exposes plaintext to the network.
pub(crate) const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8730";

/// The served prefix, matching the deployed origin's `tyde/` prefix so the
/// loader's relative fetches and the manifest's absolute `/tyde/v…/` paths
/// resolve identically here and in production.
const SERVED_PREFIX: &str = "/tyde";

/// Byte-for-byte the `ContentSecurityPolicy` from the production
/// `ResponseHeadersPolicy` (`web/deploy/cloudfront-setup.md`). Serving a
/// different policy here would mean the loader and bundle run under rules no
/// deployment actually tests. `frame-ancestors` is included because, unlike
/// the `<meta>` copy in the loader shell, a response header can enforce it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' wss:; worker-src 'self'; manifest-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Same-origin camera access for the QR scanner. Not expressible as `<meta>`.
const PERMISSIONS_POLICY: &str = "camera=(self)";

const IMMUTABLE_CACHE_CONTROL: &str = "public,max-age=31536000,immutable";
const SHELL_CACHE_CONTROL: &str = "public,max-age=60";

/// The manifest is the revocation authority, so it is never cached anywhere.
/// A stale copy would defeat a `blocked`/`minSupported` entry.
const MANIFEST_CACHE_CONTROL: &str = "no-store";
const MANIFEST_PATH: &str = "manifest.json";
const INDEX_PATH: &str = "index.html";

/// Bounds on what a bundle directory may be, so a misconfigured path (a home
/// directory, `/`) fails loudly at load instead of being read into memory.
const MAX_BUNDLE_FILES: usize = 4096;
const MAX_BUNDLE_BYTES: u64 = 128 * 1024 * 1024;

struct MobileWebAsset {
    bytes: Arc<[u8]>,
    content_type: &'static str,
}

/// Every file served under [`SERVED_PREFIX`], read once at startup and keyed by
/// its slash-separated path relative to the bundle root.
///
/// Requests are answered by looking their path up in this map; the request
/// never reaches the filesystem, so path traversal is not something the
/// handler has to defend against.
pub(crate) struct MobileWebAssets {
    files: BTreeMap<String, MobileWebAsset>,
}

impl MobileWebAssets {
    /// Reads a bundle directory laid out exactly like the deployed `tyde/`
    /// prefix: the loader shell and `manifest.json` at the root, each app
    /// bundle under `v<version>/`.
    pub(crate) fn from_dir(root: &Path) -> Result<Self, String> {
        if !root.is_dir() {
            return Err(format!(
                "mobile web bundle directory does not exist: {}",
                root.display()
            ));
        }

        let mut files = BTreeMap::new();
        let mut total_bytes = 0u64;
        read_dir_into(root, root, &mut files, &mut total_bytes)?;

        // The loader shell and the allowlist it boots from are what make this a
        // servable bundle rather than an arbitrary directory. Missing either
        // one means the path is wrong, and saying so now beats serving 404s.
        for required in [INDEX_PATH, MANIFEST_PATH] {
            if !files.contains_key(required) {
                return Err(format!(
                    "mobile web bundle directory {} has no {required}; it must be laid out like the deployed site, with the loader shell and manifest.json at its root",
                    root.display()
                ));
            }
        }

        Ok(Self { files })
    }

    pub(crate) fn asset_count(&self) -> usize {
        self.files.len()
    }

    fn get(&self, path: &str) -> Option<&MobileWebAsset> {
        self.files.get(path)
    }
}

fn read_dir_into(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, MobileWebAsset>,
    total_bytes: &mut u64,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("failed to read mobile web bundle {}: {err}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read mobile web bundle entry in {}: {err}",
                dir.display()
            )
        })?;
        let path = entry.path();
        // `file_type` does not follow symlinks, so a link out of the bundle is
        // skipped rather than pulling an unrelated tree into the served map.
        let file_type = entry.file_type().map_err(|err| {
            format!(
                "failed to stat mobile web bundle entry {}: {err}",
                path.display()
            )
        })?;

        let name = entry.file_name();
        let name = name.to_string_lossy();
        // AppleDouble sidecars and Trunk's transient stage dir are build
        // droppings, never served artifacts. Skipped for the same reason
        // `web/deploy/generate-manifest.mjs` skips them.
        if name.starts_with("._") || name == ".stage" {
            continue;
        }

        if file_type.is_dir() {
            read_dir_into(root, &path, files, total_bytes)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let relative = path.strip_prefix(root).map_err(|_| {
            format!(
                "mobile web bundle entry escaped its root: {}",
                path.display()
            )
        })?;
        let key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");

        let bytes = std::fs::read(&path).map_err(|err| {
            format!(
                "failed to read mobile web bundle file {}: {err}",
                path.display()
            )
        })?;
        *total_bytes += bytes.len() as u64;
        if *total_bytes > MAX_BUNDLE_BYTES || files.len() >= MAX_BUNDLE_FILES {
            return Err(format!(
                "mobile web bundle directory {} is larger than a bundle can be ({MAX_BUNDLE_FILES} files / {MAX_BUNDLE_BYTES} bytes); check that the path points at a built bundle",
                root.display()
            ));
        }

        files.insert(
            key,
            MobileWebAsset {
                content_type: content_type_for(&path),
                bytes: Arc::from(bytes),
            },
        );
    }

    Ok(())
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        // Anything but `application/wasm` breaks
        // `WebAssembly.instantiateStreaming`.
        "wasm" => "application/wasm",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        // Inert under `X-Content-Type-Options: nosniff`.
        _ => "application/octet-stream",
    }
}

fn cache_control_for(path: &str) -> &'static str {
    if path == MANIFEST_PATH {
        MANIFEST_CACHE_CONTROL
    } else if path.starts_with('v') && path.contains('/') {
        // `v<version>/…` bundles are immutable by construction: a new build
        // publishes under a new version rather than replacing these bytes.
        IMMUTABLE_CACHE_CONTROL
    } else {
        SHELL_CACHE_CONTROL
    }
}

/// Resolves a request path to a bundle key, or `None` when it falls outside the
/// served prefix.
fn asset_key(uri_path: &str) -> Option<String> {
    let rest = uri_path.strip_prefix(SERVED_PREFIX)?;
    let rest = match rest {
        "" => "",
        other => other.strip_prefix('/')?,
    };
    // A directory request serves that directory's index, matching how a static
    // origin resolves `/tyde/` to the loader shell.
    if rest.is_empty() {
        return Some(INDEX_PATH.to_owned());
    }
    if let Some(dir) = rest.strip_suffix('/') {
        return Some(format!("{dir}/{INDEX_PATH}"));
    }
    Some(rest.to_owned())
}

fn apply_common_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );
}

async fn serve(State(assets): State<Arc<MobileWebAssets>>, method: Method, uri: Uri) -> Response {
    let mut response = serve_inner(&assets, &method, uri.path());
    apply_common_headers(&mut response);
    response
}

fn serve_inner(assets: &MobileWebAssets, method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }

    // `/tyde` without the trailing slash would make the loader's relative
    // `./manifest.json` resolve one level too high.
    if path == SERVED_PREFIX {
        return Response::builder()
            .status(StatusCode::PERMANENT_REDIRECT)
            .header(header::LOCATION, format!("{SERVED_PREFIX}/"))
            .body(Body::empty())
            .expect("static redirect response must build");
    }

    let Some(key) = asset_key(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(asset) = assets.get(&key) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.bytes.to_vec())
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type)
        .header(header::CONTENT_LENGTH, asset.bytes.len())
        .header(header::CACHE_CONTROL, cache_control_for(&key))
        .body(body)
        .expect("static asset response must build")
}

fn router(assets: Arc<MobileWebAssets>) -> Router {
    Router::new().fallback(serve).with_state(assets)
}

/// A running direct-hosting server. Dropping it stops serving.
pub(crate) struct MobileHttpServer {
    local_addr: SocketAddr,
    shutdown: CancellationToken,
}

impl MobileHttpServer {
    /// Binds `bind_addr` synchronously so an unusable address is reported to
    /// the caller rather than discovered later in a detached task, then serves
    /// on its own thread and runtime.
    pub(crate) fn start(
        bind_addr: SocketAddr,
        assets: Arc<MobileWebAssets>,
    ) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(bind_addr)
            .map_err(|err| format!("failed to bind mobile web server on {bind_addr}: {err}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("failed to set mobile web listener nonblocking: {err}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|err| format!("failed to read mobile web listener addr: {err}"))?;

        let shutdown = CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("tyde-mobile-http".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        tracing::error!(error = %err, "failed to build mobile web server runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(err) => {
                            tracing::error!(error = %err, "failed to adopt mobile web listener");
                            return;
                        }
                    };
                    let served = axum::serve(listener, router(assets))
                        .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                        .await;
                    if let Err(err) = served {
                        tracing::error!(error = %err, "mobile web server stopped");
                    }
                });
            })
            .map_err(|err| format!("failed to spawn mobile web server thread: {err}"))?;

        Ok(Self {
            local_addr,
            shutdown,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for MobileHttpServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Resolves the configured bind address, falling back to the documented
/// default only when the setting is absent — a present-but-unparseable value
/// is an error, never silently replaced.
pub(crate) fn resolve_bind_addr(configured: Option<&str>) -> Result<SocketAddr, String> {
    let raw = configured.map(str::trim).filter(|value| !value.is_empty());
    match raw {
        Some(value) => value.parse().map_err(|err| {
            format!("mobile direct hosting address \"{value}\" is not a valid address: {err}")
        }),
        None => Ok(DEFAULT_BIND_ADDR
            .parse()
            .expect("default mobile web bind addr must parse")),
    }
}
