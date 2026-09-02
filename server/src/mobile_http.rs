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
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use protocol::{MobileAccessErrorCode, MobileDirectErrorResponse, MobileDirectPairRequest};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::mobile_access::MobileAccessCommand;

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

const PAIR_PATH: &str = "/tyde/pair";
const WS_PATH: &str = "/tyde/ws";

/// Subprotocol the client selects to name the wire it speaks.
const WS_PROTOCOL: &str = "tyde.v1";
/// Prefix of the second subprotocol entry, which carries the device token.
///
/// The browser WebSocket API cannot set request headers, and a token in the
/// query string would be written to every reverse proxy access log on the way
/// in. Subprotocols travel in `Sec-WebSocket-Protocol`, which proxies do not
/// log by default, so this is where the credential goes.
const WS_TOKEN_PREFIX: &str = "tyde.token.";

/// Frames larger than this are refused rather than buffered. The protocol's own
/// records are far smaller; anything approaching this is a client that has lost
/// framing, and growing a buffer for it would be the bug.
const WS_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Depth of the byte queues bridging the WebSocket task and the protocol
/// bridge. Bounded so a stalled reader applies backpressure to the socket
/// instead of growing without limit.
const WS_QUEUE_DEPTH: usize = 32;

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

async fn serve(State(state): State<MobileHttpState>, method: Method, uri: Uri) -> Response {
    let mut response = serve_inner(&state.assets, &method, uri.path());
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

/// A byte stream over a WebSocket, so `accept` can run the ordinary Tyde
/// handshake on it exactly as it does over a Unix socket or an MQTT session.
///
/// The socket itself stays on the HTTP server's runtime and talks to this
/// through channels. That is not indirection for its own sake: the mobile
/// access actor runs on its own runtime, and hyper's upgraded IO is bound to
/// the reactor that accepted it, so polling the socket from the actor's thread
/// would never wake.
struct WebSocketDuplex {
    inbound: mpsc::Receiver<Vec<u8>>,
    outbound: mpsc::Sender<Vec<u8>>,
    /// Remainder of a frame that was larger than the last read buffer.
    partial: Vec<u8>,
    partial_offset: usize,
    write_permit: Option<mpsc::OwnedPermit<Vec<u8>>>,
}

impl WebSocketDuplex {
    fn new(inbound: mpsc::Receiver<Vec<u8>>, outbound: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            inbound,
            outbound,
            partial: Vec::new(),
            partial_offset: 0,
            write_permit: None,
        }
    }
}

impl AsyncRead for WebSocketDuplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.partial_offset >= self.partial.len() {
            match self.inbound.poll_recv(cx) {
                // A closed inbound channel is the socket having ended, which is
                // a clean EOF for the protocol reader above.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(frame)) => {
                    self.partial = frame;
                    self.partial_offset = 0;
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        let available = &self.partial[self.partial_offset..];
        let take = available.len().min(buf.remaining());
        buf.put_slice(&available[..take]);
        self.partial_offset += take;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for WebSocketDuplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let permit = match self.write_permit.take() {
            Some(permit) => permit,
            None => match self.outbound.clone().try_reserve_owned() {
                Ok(permit) => permit,
                Err(mpsc::error::TrySendError::Full(sender)) => {
                    // Wake when the socket task drains one frame.
                    let waker = cx.waker().clone();
                    tokio::spawn(async move {
                        let _ = sender.reserve_owned().await;
                        waker.wake();
                    });
                    return Poll::Pending;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "mobile websocket closed",
                    )));
                }
            },
        };
        permit.send(buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Every accepted write has already been handed to the socket task.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.write_permit = None;
        self.inbound.close();
        Poll::Ready(Ok(()))
    }
}

/// Pumps an upgraded WebSocket into and out of the channels a
/// [`WebSocketDuplex`] reads. Ends as soon as either direction does, so a
/// half-dead socket cannot leave the protocol bridge waiting.
async fn run_websocket_pump(
    mut socket: WebSocket,
    mut outbound: mpsc::Receiver<Vec<u8>>,
    inbound: mpsc::Sender<Vec<u8>>,
) {
    loop {
        tokio::select! {
            received = socket.recv() => {
                match received {
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.len() > WS_MAX_FRAME_BYTES {
                            tracing::warn!(
                                bytes = bytes.len(),
                                "mobile websocket frame exceeds the maximum; closing"
                            );
                            return;
                        }
                        if inbound.send(bytes.into()).await.is_err() {
                            return;
                        }
                    }
                    // The protocol is binary. A text frame means the peer is
                    // not speaking it, which is worth ending rather than
                    // silently dropping.
                    Some(Ok(Message::Text(_))) => {
                        tracing::warn!("mobile websocket sent a text frame; closing");
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Err(error)) => {
                        tracing::debug!(%error, "mobile websocket read failed");
                        return;
                    }
                }
            }
            sending = outbound.recv() => {
                let Some(bytes) = sending else {
                    return;
                };
                if socket.send(Message::Binary(bytes.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Reads the device token out of the offered subprotocols.
fn device_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let offered = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    offered
        .split(',')
        .map(str::trim)
        .find_map(|entry| entry.strip_prefix(WS_TOKEN_PREFIX))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

fn direct_error(status: StatusCode, code: MobileAccessErrorCode, message: &str) -> Response {
    let mut response = (
        status,
        Json(MobileDirectErrorResponse {
            code,
            message: message.to_owned(),
        }),
    )
        .into_response();
    apply_common_headers(&mut response);
    response
}

/// Exchanges a pairing offer secret for a durable device token.
async fn pair(
    State(state): State<MobileHttpState>,
    Json(request): Json<MobileDirectPairRequest>,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .mobile_access
        .send(MobileAccessCommand::RedeemDirectPairing {
            request: Box::new(request),
            reply: reply_tx,
        })
        .is_err()
    {
        return direct_error(
            StatusCode::SERVICE_UNAVAILABLE,
            MobileAccessErrorCode::Internal,
            "this host is shutting down",
        );
    }

    match reply_rx.await {
        Ok(Ok(response)) => {
            let mut response = Json(response).into_response();
            apply_common_headers(&mut response);
            response
        }
        Ok(Err(failure)) => {
            let (code, message) = failure.into_parts();
            direct_error(StatusCode::FORBIDDEN, code, &message)
        }
        Err(_) => direct_error(
            StatusCode::SERVICE_UNAVAILABLE,
            MobileAccessErrorCode::Internal,
            "this host is shutting down",
        ),
    }
}

/// Upgrades an authenticated device to a protocol connection.
async fn websocket(
    State(state): State<MobileHttpState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = device_token_from_headers(&headers) else {
        return direct_error(
            StatusCode::UNAUTHORIZED,
            MobileAccessErrorCode::RepairRequired,
            "this connection carried no device token; pair with the host again",
        );
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .mobile_access
        .send(MobileAccessCommand::AuthenticateDirectDevice {
            token,
            reply: reply_tx,
        })
        .is_err()
    {
        return direct_error(
            StatusCode::SERVICE_UNAVAILABLE,
            MobileAccessErrorCode::Internal,
            "this host is shutting down",
        );
    }
    let Ok(Some(device_id)) = reply_rx.await else {
        return direct_error(
            StatusCode::UNAUTHORIZED,
            MobileAccessErrorCode::RepairRequired,
            "this device is not paired with the host; pair again",
        );
    };

    let mobile_access = state.mobile_access.clone();
    let mut response = upgrade
        .protocols([WS_PROTOCOL])
        .on_upgrade(move |socket| async move {
            let (inbound_tx, inbound_rx) = mpsc::channel(WS_QUEUE_DEPTH);
            let (outbound_tx, outbound_rx) = mpsc::channel(WS_QUEUE_DEPTH);
            let duplex = WebSocketDuplex::new(inbound_rx, outbound_tx);
            if mobile_access
                .send(MobileAccessCommand::DeviceTransportConnected {
                    device_id,
                    stream: Box::new(duplex),
                })
                .is_err()
            {
                return;
            }
            run_websocket_pump(socket, outbound_rx, inbound_tx).await;
        });
    apply_common_headers(&mut response);
    response
}

#[derive(Clone)]
struct MobileHttpState {
    assets: Arc<MobileWebAssets>,
    mobile_access: mpsc::UnboundedSender<MobileAccessCommand>,
}

fn router(
    assets: Arc<MobileWebAssets>,
    mobile_access: mpsc::UnboundedSender<MobileAccessCommand>,
) -> Router {
    Router::new()
        .route(PAIR_PATH, post(pair))
        .route(WS_PATH, get(websocket))
        .fallback(serve)
        .with_state(MobileHttpState {
            assets,
            mobile_access,
        })
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
        mobile_access: mpsc::UnboundedSender<MobileAccessCommand>,
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
                    let served = axum::serve(listener, router(assets, mobile_access))
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
