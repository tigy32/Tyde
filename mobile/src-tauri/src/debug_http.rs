//! HTTP debug server for inspecting the mobile webview.
//!
//! The page installs a tiny JavaScript bridge before the WASM app loads. The
//! bridge short-polls this loopback HTTP task for debug requests and submits
//! responses via HTTP POST. The short-poll transport is intentionally used here
//! because WKWebView closes long-lived loopback streams during desktop dev runs.
//!
//! Endpoints:
//! - GET /ping — liveness check
//! - GET /eval?js=<encoded> — evaluate JS in the webview
//! - GET /screenshot — PNG DOM screenshot rendered by headless Chrome
//! - GET /screenshot/native — PNG window screenshot captured by the OS during macOS dev runs
//! - GET /screenshot.json — screenshot metadata and base64 PNG
//! - GET /dom — body outerHTML
//! - GET /status — app state summary
//! - GET /test/nav — click every bottom navigation tab and verify the title

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use devtools_protocol::{
    UiDebugRequest, UiDebugRequestEvent, UiDebugResponse, UiDebugResponseSubmission,
};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, timeout};

const LISTEN_ADDR: &str = "127.0.0.1:9820";
const UI_DEBUG_DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Bridge state shared between the HTTP server (sends requests) and the
/// frontend HTTP bridge (delivers responses from the webview).
pub struct UiDebugBridgeState {
    ready: AtomicBool,
    active_session: Mutex<Option<String>>,
    queued: Mutex<HashMap<String, VecDeque<UiDebugRequestEvent>>>,
    poll_counts: Mutex<HashMap<String, u64>>,
    pending: Mutex<HashMap<String, oneshot::Sender<UiDebugResponse>>>,
}

impl Default for UiDebugBridgeState {
    fn default() -> Self {
        Self {
            ready: AtomicBool::new(false),
            active_session: Mutex::new(None),
            queued: Mutex::new(HashMap::new()),
            poll_counts: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl UiDebugBridgeState {
    pub async fn mark_ready(&self, session_id: String) {
        tracing::info!("ui debug bridge: frontend marked ready session_id={session_id}");
        self.ready.store(true, Ordering::Release);
        *self.active_session.lock().await = Some(session_id.clone());
        let mut queued = self.queued.lock().await;
        queued.clear();
        queued.entry(session_id).or_default();
        self.poll_counts.lock().await.clear();
    }

    pub async fn health_json(&self) -> serde_json::Value {
        let queue_counts = self
            .queued
            .lock()
            .await
            .iter()
            .map(|(session, queue)| {
                serde_json::json!({
                    "session": session,
                    "queued": queue.len(),
                })
            })
            .collect::<Vec<_>>();
        let queued_requests = queue_counts
            .iter()
            .filter_map(|entry| entry.get("queued").and_then(|value| value.as_u64()))
            .sum::<u64>();
        let pending = self.pending.lock().await.len();
        let active_session = self.active_session.lock().await.clone();
        let poll_counts = self.poll_counts.lock().await.clone();
        serde_json::json!({
            "status": "ok",
            "ready": self.ready.load(Ordering::Acquire),
            "active_session": active_session,
            "queued_requests": queued_requests,
            "queues": queue_counts,
            "poll_counts": poll_counts,
            "pending_requests": pending,
        })
    }

    pub async fn dispatch_request(
        &self,
        _app: &tauri::AppHandle,
        request: UiDebugRequest,
    ) -> Result<UiDebugResponse, String> {
        if !self.ready.load(Ordering::Acquire) {
            return Err("UI debug frontend is not ready".to_string());
        }

        let timeout_ms = request_timeout_ms(&request);
        let request_id = uuid::Uuid::new_v4().to_string();
        let active_session = self
            .active_session
            .lock()
            .await
            .clone()
            .ok_or("UI debug frontend has no active session".to_string())?;
        let (tx, rx) = oneshot::channel();
        {
            let previous = self.pending.lock().await.insert(request_id.clone(), tx);
            assert!(
                previous.is_none(),
                "duplicate pending UI debug request_id {request_id}"
            );
        }

        let event = UiDebugRequestEvent {
            request_id: request_id.clone(),
            request,
        };
        {
            let mut queued = self.queued.lock().await;
            queued
                .entry(active_session.clone())
                .or_default()
                .push_back(event.clone());
        }
        if let Err(error) = _app.emit("tyde://ui-debug-request", event) {
            tracing::warn!(
                "ui debug bridge: Tauri event emit failed for request_id={request_id}: {error}"
            );
        }
        tracing::info!(
            "ui debug bridge: queued request_id={request_id} session_id={active_session}"
        );

        match timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&request_id);
                self.remove_queued_request(&request_id).await;
                Err("UI debug frontend dropped the response channel".to_string())
            }
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                self.remove_queued_request(&request_id).await;
                Err(format!("UI debug request timed out after {timeout_ms}ms"))
            }
        }
    }

    async fn remove_queued_request(&self, request_id: &str) {
        let mut queued = self.queued.lock().await;
        for queue in queued.values_mut() {
            queue.retain(|event| event.request_id != request_id);
        }
    }

    pub async fn take_request(&self, session_id: String) -> Option<UiDebugRequestEvent> {
        *self
            .poll_counts
            .lock()
            .await
            .entry(session_id.clone())
            .or_default() += 1;
        if self
            .active_session
            .lock()
            .await
            .as_deref()
            .is_none_or(|active| active != session_id)
        {
            return None;
        }

        let mut queued = self.queued.lock().await;
        let event = queued.get_mut(&session_id).and_then(VecDeque::pop_front);
        if let Some(event) = &event {
            tracing::info!(
                "ui debug bridge: frontend took request_id={} session_id={session_id}",
                event.request_id
            );
        }
        event
    }

    pub async fn submit_response(
        &self,
        submission: UiDebugResponseSubmission,
    ) -> Result<(), String> {
        let tx = self.pending.lock().await.remove(&submission.request_id);
        let Some(tx) = tx else {
            return Err(format!(
                "unknown UI debug request_id '{}'",
                submission.request_id
            ));
        };
        self.remove_queued_request(&submission.request_id).await;
        tx.send(submission.response)
            .map_err(|_| "failed to deliver UI debug response".to_string())
    }
}

struct DebugHttpResponse {
    content_type: &'static str,
    body: Vec<u8>,
}

impl DebugHttpResponse {
    fn text(body: impl Into<String>) -> Self {
        Self {
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
        }
    }

    fn json(value: serde_json::Value) -> Self {
        match serde_json::to_vec_pretty(&value) {
            Ok(body) => Self {
                content_type: "application/json; charset=utf-8",
                body,
            },
            Err(error) => Self::text(format!("error: failed to serialize JSON: {error}")),
        }
    }

    fn png(body: Vec<u8>) -> Self {
        Self {
            content_type: "image/png",
            body,
        }
    }
}

pub fn start(app_handle: tauri::AppHandle, bridge: Arc<UiDebugBridgeState>) {
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(LISTEN_ADDR).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("debug HTTP bind failed: {e}");
                return;
            }
        };
        tracing::info!("debug HTTP on {LISTEN_ADDR}");

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let app = app_handle.clone();
            let bridge = bridge.clone();
            tauri::async_runtime::spawn(async move {
                handle_connection(stream, &app, &bridge).await;
            });
        }
    });
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
) {
    let request = match read_http_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!("debug HTTP: failed to read request: {error}");
            return;
        }
    };

    let log_request = !request.path.starts_with("/ui-debug/poll");
    if log_request {
        tracing::info!("debug HTTP: {} {}", request.method, request.path);
    }

    let response = route(&request.method, &request.path, &request.body, app, bridge).await;
    if log_request || response.body.as_slice() != b"null" {
        tracing::info!(
            "debug HTTP: {} {} -> {} bytes",
            request.method,
            request.path,
            response.body.len()
        );
    }

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(&response.body).await;
}

struct DebugHttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Result<DebugHttpRequest, String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed to read request: {error}"))?;
        if n == 0 {
            return Err("connection closed before headers completed".to_string());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(index) = find_header_end(&bytes) {
            break index;
        }
        if bytes.len() > 64 * 1024 {
            return Err("request headers exceeded 64 KiB".to_string());
        }
    };

    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > 20 * 1024 * 1024 {
        return Err("request body exceeded 20 MiB".to_string());
    }

    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let mut chunk = [0_u8; 8192];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed to read request body: {error}"))?;
        if n == 0 {
            return Err("connection closed before body completed".to_string());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }

    Ok(DebugHttpRequest {
        method,
        path,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn route(
    method: &str,
    path: &str,
    body: &[u8],
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
) -> DebugHttpResponse {
    if method == "OPTIONS" {
        return DebugHttpResponse::text("");
    }
    if path.starts_with("/ui-debug/ready") && method == "POST" {
        let Some(session_id) = query_param(path, "session") else {
            return DebugHttpResponse::json(serde_json::json!({
                "ok": false,
                "error": "missing UI debug session id",
            }));
        };
        bridge.mark_ready(session_id).await;
        return DebugHttpResponse::json(serde_json::json!({ "ok": true }));
    }
    if path.starts_with("/ui-debug/poll") {
        let Some(session_id) = query_param(path, "session") else {
            return DebugHttpResponse::json(serde_json::json!({
                "kind": "error",
                "message": "missing UI debug session id",
            }));
        };
        let request = bridge.take_request(session_id).await;
        return DebugHttpResponse::json(serde_json::to_value(request).unwrap_or_else(
            |error| serde_json::json!({ "kind": "error", "message": error.to_string() }),
        ));
    }
    if path == "/ui-debug/response" && method == "POST" {
        return match serde_json::from_slice::<UiDebugResponseSubmission>(body) {
            Ok(submission) => match bridge.submit_response(submission).await {
                Ok(()) => DebugHttpResponse::json(serde_json::json!({ "ok": true })),
                Err(error) => DebugHttpResponse::json(serde_json::json!({
                    "ok": false,
                    "error": error,
                })),
            },
            Err(error) => DebugHttpResponse::json(serde_json::json!({
                "ok": false,
                "error": format!("failed to parse UI debug response: {error}"),
            })),
        };
    }
    if path == "/" {
        return DebugHttpResponse::text(
            "GET /health /ping /dom /status /screenshot /screenshot/native /screenshot.json /test/nav /eval?js=<encoded>",
        );
    }
    if path == "/health" {
        return DebugHttpResponse::json(bridge.health_json().await);
    }
    if path == "/window/metrics" {
        return DebugHttpResponse::json(window_metrics_json(app));
    }
    if path == "/ping" {
        return DebugHttpResponse::text(
            match bridge.dispatch_request(app, UiDebugRequest::Ping).await {
                Ok(resp) => format!("{resp:?}"),
                Err(e) => format!("error: {e}"),
            },
        );
    }
    if path == "/dom" {
        return DebugHttpResponse::text(
            eval(
                app,
                bridge,
                "return document.body ? document.body.outerHTML.substring(0,10000) : 'no body'",
                None,
            )
            .await,
        );
    }
    if path == "/status" {
        return DebugHttpResponse::text(
            eval(
                app,
                bridge,
                r#"
            var a = document.querySelector('.mobile-app');
            return JSON.stringify({
                title: document.title,
                tauri: !!window.__TAURI__,
                appEl: a ? 'found' : 'missing',
                appKids: a ? a.childElementCount : -1,
                appHTML: a ? a.innerHTML.substring(0,3000) : 'none',
                bodyKids: document.body.childElementCount,
            }, null, 2);
        "#,
                None,
            )
            .await,
        );
    }
    if path.starts_with("/screenshot/native") {
        return match native_screenshot_png(app).await {
            Ok(png) => DebugHttpResponse::png(png),
            Err(error) => DebugHttpResponse::text(format!("error: {error}")),
        };
    }
    if path.starts_with("/screenshot/browser") {
        return match browser_screenshot_png(app, bridge).await {
            Ok(png) => DebugHttpResponse::png(png),
            Err(error) => DebugHttpResponse::text(format!("error: {error}")),
        };
    }
    if path.starts_with("/screenshot/dom") {
        let max_dimension = query_param(path, "maxDimension").and_then(|value| value.parse().ok());
        return match screenshot_png(app, bridge, max_dimension).await {
            Ok(png) => DebugHttpResponse::png(png),
            Err(error) => DebugHttpResponse::text(format!("error: {error}")),
        };
    }
    if path.starts_with("/screenshot.json") {
        let max_dimension = query_param(path, "maxDimension").and_then(|value| value.parse().ok());
        return match screenshot_json(app, bridge, max_dimension).await {
            Ok(value) => DebugHttpResponse::json(value),
            Err(error) => DebugHttpResponse::text(format!("error: {error}")),
        };
    }
    if path.starts_with("/screenshot") {
        return match browser_screenshot_png(app, bridge).await {
            Ok(png) => DebugHttpResponse::png(png),
            Err(error) => DebugHttpResponse::text(format!("error: {error}")),
        };
    }
    if path == "/test/nav" {
        return DebugHttpResponse::text(nav_smoke_test(app, bridge).await);
    }
    if path.starts_with("/eval?") && method == "GET" {
        let Some(js) = query_param(path, "js") else {
            return DebugHttpResponse::text("error: missing js query parameter");
        };
        let timeout_ms = query_param(path, "timeoutMs").and_then(|value| value.parse().ok());
        return DebugHttpResponse::text(eval(app, bridge, &js, timeout_ms).await);
    }
    if (path == "/eval" || path.starts_with("/eval?")) && method == "POST" {
        let js = match std::str::from_utf8(body) {
            Ok(js) => js,
            Err(error) => {
                return DebugHttpResponse::text(format!("error: eval body is not UTF-8: {error}"));
            }
        };
        let timeout_ms = query_param(path, "timeoutMs").and_then(|value| value.parse().ok());
        return DebugHttpResponse::text(eval(app, bridge, js, timeout_ms).await);
    }
    DebugHttpResponse::text("unknown endpoint")
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return urlencoding::decode(&value.replace('+', " "))
                .ok()
                .map(|value| value.into_owned());
        }
    }
    None
}

fn window_metrics_json(app: &tauri::AppHandle) -> serde_json::Value {
    let Some(window) = app.get_webview_window("main") else {
        return serde_json::json!({
            "ok": false,
            "error": "main webview window not found",
        });
    };
    let position = window.outer_position();
    let size = window.outer_size();
    let scale_factor = window.scale_factor();
    let current_monitor = match window.current_monitor() {
        Ok(Some(monitor)) => serde_json::json!({
            "name": monitor.name(),
            "scale_factor": monitor.scale_factor(),
            "position": {
                "x": monitor.position().x,
                "y": monitor.position().y,
            },
            "size": {
                "width": monitor.size().width,
                "height": monitor.size().height,
            },
        }),
        Ok(None) => serde_json::Value::Null,
        Err(error) => serde_json::json!({ "error": error.to_string() }),
    };
    serde_json::json!({
        "ok": true,
        "outer_position": position.as_ref().map(|position| serde_json::json!({
            "x": position.x,
            "y": position.y,
        })).unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() })),
        "outer_size": size.as_ref().map(|size| serde_json::json!({
            "width": size.width,
            "height": size.height,
        })).unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() })),
        "scale_factor": scale_factor.map_err(|error| error.to_string()),
        "current_monitor": current_monitor,
    })
}

async fn native_screenshot_png(app: &tauri::AppHandle) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        let window = app
            .get_webview_window("main")
            .ok_or("main webview window not found")?;
        let position = window
            .outer_position()
            .map_err(|error| format!("failed to read window position: {error}"))?;
        let size = window
            .outer_size()
            .map_err(|error| format!("failed to read window size: {error}"))?;
        if size.width == 0 || size.height == 0 {
            return Err("window size is zero".to_string());
        }
        let scale_factor = window
            .scale_factor()
            .map_err(|error| format!("failed to read window scale factor: {error}"))?;

        let path =
            std::env::temp_dir().join(format!("tyde-mobile-native-{}.png", uuid::Uuid::new_v4()));
        // `outer_position`/`outer_size` are physical pixels on macOS while
        // `screencapture -R` expects display points.
        let rect = format!(
            "{},{},{},{}",
            (position.x as f64 / scale_factor).round() as i32,
            (position.y as f64 / scale_factor).round() as i32,
            (size.width as f64 / scale_factor).round() as u32,
            (size.height as f64 / scale_factor).round() as u32
        );
        let status = std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-t", "png", "-R"])
            .arg(&rect)
            .arg(&path)
            .status()
            .map_err(|error| format!("failed to start screencapture: {error}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            return Err(format!("screencapture failed with status {status}"));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("failed to read native screenshot: {error}"))?;
        let _ = std::fs::remove_file(&path);
        Ok(bytes)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("native screenshot is only implemented for macOS dev runs".to_string())
    }
}

async fn browser_screenshot_png(
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
) -> Result<Vec<u8>, String> {
    let snapshot = eval(
        app,
        bridge,
        r#"
            const clone = document.body.cloneNode(true);
            clone.querySelectorAll('script').forEach((node) => node.remove());
            clone.querySelectorAll('textarea').forEach((node) => {
                node.textContent = node.value || '';
            });
            clone.querySelectorAll('input').forEach((node) => {
                if (node.value) {
                    node.setAttribute('value', node.value);
                } else {
                    node.removeAttribute('value');
                }
            });
            return JSON.stringify({
                body: clone.innerHTML,
                theme: document.body.getAttribute('data-theme') || 'dark',
                width: window.innerWidth || document.documentElement.clientWidth || 390,
                height: window.innerHeight || document.documentElement.clientHeight || 816
            });
        "#,
        Some(10_000),
    )
    .await;
    if snapshot.starts_with("error: ") {
        return Err(snapshot);
    }
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot)
        .map_err(|error| format!("failed to parse browser screenshot snapshot: {error}"))?;
    let body = snapshot
        .get("body")
        .and_then(|value| value.as_str())
        .ok_or("browser screenshot snapshot did not include body HTML")?;
    let theme = snapshot
        .get("theme")
        .and_then(|value| value.as_str())
        .unwrap_or("dark");
    let width = snapshot
        .get("width")
        .and_then(|value| value.as_u64())
        .unwrap_or(390)
        .clamp(1, 4096);
    let height = snapshot
        .get("height")
        .and_then(|value| value.as_u64())
        .unwrap_or(816)
        .clamp(1, 4096);
    let stylesheet = find_mobile_stylesheet()?;
    let stylesheet_uri = file_uri(&stylesheet);
    let escaped_theme = escape_html_attr(theme);
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="{stylesheet_uri}">
<style>body {{ background:#0b0f14; color:#e8edf3; margin:0; }}</style>
</head>
<body data-theme="{escaped_theme}">{body}</body>
</html>"#
    );

    let id = uuid::Uuid::new_v4();
    let html_path = std::env::temp_dir().join(format!("tyde-mobile-screenshot-{id}.html"));
    let png_path = std::env::temp_dir().join(format!("tyde-mobile-screenshot-{id}.png"));
    let profile_path = std::env::temp_dir().join(format!("tyde-mobile-chrome-profile-{id}"));
    std::fs::write(&html_path, html)
        .map_err(|error| format!("failed to write browser screenshot HTML: {error}"))?;

    let chrome = chrome_path()?;
    let mut child = std::process::Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        .arg("--no-first-run")
        .arg(format!("--user-data-dir={}", profile_path.display()))
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--screenshot={}", png_path.display()))
        .arg(file_uri(&html_path))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start Chrome screenshot renderer: {error}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut last_png_len = None;
    loop {
        if let Ok(metadata) = std::fs::metadata(&png_path) {
            let len = metadata.len();
            if len > 0 && last_png_len == Some(len) {
                if child
                    .try_wait()
                    .map_err(|error| format!("failed to poll Chrome screenshot renderer: {error}"))?
                    .is_none()
                {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                break;
            }
            last_png_len = Some(len);
        }

        match child
            .try_wait()
            .map_err(|error| format!("failed to wait for Chrome screenshot renderer: {error}"))?
        {
            Some(status) if status.success() => break,
            Some(status) => {
                cleanup_browser_screenshot_files(&html_path, &png_path, &profile_path);
                return Err(format!(
                    "Chrome screenshot renderer failed with status {status}"
                ));
            }
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                cleanup_browser_screenshot_files(&html_path, &png_path, &profile_path);
                return Err(
                    "Chrome screenshot renderer timed out before writing a stable PNG".to_string(),
                );
            }
        }
    }

    let png = std::fs::read(&png_path)
        .map_err(|error| format!("failed to read browser screenshot PNG: {error}"))?;
    cleanup_browser_screenshot_files(&html_path, &png_path, &profile_path);
    Ok(png)
}

fn cleanup_browser_screenshot_files(
    html_path: &std::path::Path,
    png_path: &std::path::Path,
    profile_path: &std::path::Path,
) {
    let _ = std::fs::remove_file(html_path);
    let _ = std::fs::remove_file(png_path);
    let _ = std::fs::remove_dir_all(profile_path);
}

fn find_mobile_stylesheet() -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?;
    let candidates = [
        cwd.join("../../mobile-frontend/dist"),
        cwd.join("../mobile-frontend/dist"),
        cwd.join("mobile-frontend/dist"),
    ];
    for dir in candidates {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("styles-") && name.ends_with(".css") {
                return Ok(path);
            }
        }
    }
    Err("failed to locate mobile-frontend/dist/styles-*.css".to_string())
}

fn chrome_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("TYDE_CHROME_PATH") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }
    for path in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }
    Err("failed to locate Chrome; set TYDE_CHROME_PATH for /screenshot".to_string())
}

fn file_uri(path: &std::path::Path) -> String {
    format!(
        "file://{}",
        path.to_string_lossy()
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('#', "%23")
    )
}

fn escape_html_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn screenshot_json(
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
    max_dimension: Option<u32>,
) -> Result<serde_json::Value, String> {
    match bridge
        .dispatch_request(app, UiDebugRequest::CaptureScreenshot { max_dimension })
        .await?
    {
        UiDebugResponse::CaptureScreenshotResult {
            png_base64,
            width,
            height,
        } => Ok(serde_json::json!({
            "kind": "capture_screenshot_result",
            "width": width,
            "height": height,
            "png_base64": png_base64,
        })),
        UiDebugResponse::Error { message } => Err(message),
        other => Err(format!("unexpected screenshot response: {other:?}")),
    }
}

async fn screenshot_png(
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
    max_dimension: Option<u32>,
) -> Result<Vec<u8>, String> {
    let value = screenshot_json(app, bridge, max_dimension).await?;
    let encoded = value
        .get("png_base64")
        .and_then(|value| value.as_str())
        .ok_or("screenshot response did not include png_base64")?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| format!("failed to decode screenshot PNG: {err}"))
}

async fn nav_smoke_test(app: &tauri::AppHandle, bridge: &UiDebugBridgeState) -> String {
    eval(
        app,
        bridge,
        r#"
        const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
        const waitForTitle = async (expectedTitle) => {
            for (let i = 0; i < 20; i++) {
                const title = document.querySelector(".view-title")?.textContent ?? null;
                if (title === expectedTitle) return title;
                await sleep(25);
            }
            return document.querySelector(".view-title")?.textContent ?? null;
        };
        const expected = [
            ["Home", "Tyde"],
            ["Agents", "Agents"],
            ["Sessions", "Sessions"],
            ["Projects", "Projects"],
            ["Settings", "Settings"],
        ];
        const steps = [];
        for (const [label, expectedTitle] of expected) {
            const button = Array.from(document.querySelectorAll(".bottom-nav button"))
                .find((candidate) => candidate.textContent.includes(label));
            if (!button) {
                steps.push({ label, ok: false, error: "missing bottom-nav button" });
                continue;
            }
            button.click();
            const title = await waitForTitle(expectedTitle);
            const text = document.querySelector(".view-body")?.textContent ?? "";
            steps.push({
                label,
                expectedTitle,
                title,
                bodyTextLength: text.length,
                ok: title === expectedTitle && text.length > 0,
            });
        }
        const app = document.querySelector(".mobile-app");
        const rect = app ? app.getBoundingClientRect() : null;
        return JSON.stringify({
            ok: steps.every((step) => step.ok),
            steps,
            appRect: rect ? {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            } : null,
            connectionText: document.querySelector(".connection-banner")?.textContent ?? null,
        }, null, 2);
    "#,
        None,
    )
    .await
}

async fn eval(
    app: &tauri::AppHandle,
    bridge: &UiDebugBridgeState,
    js: &str,
    timeout_ms: Option<u64>,
) -> String {
    let request = UiDebugRequest::Evaluate {
        expression: js.to_string(),
        timeout_ms,
    };
    match bridge.dispatch_request(app, request).await {
        Ok(UiDebugResponse::EvaluateResult { value }) => match value {
            serde_json::Value::String(s) => s,
            other => match serde_json::to_string_pretty(&other) {
                Ok(serialized) => serialized,
                Err(error) => format!("error: failed to serialize eval response: {error}"),
            },
        },
        Ok(UiDebugResponse::Error { message }) => format!("error: {message}"),
        Ok(other) => format!("unexpected response: {other:?}"),
        Err(e) => format!("error: {e}"),
    }
}

fn request_timeout_ms(request: &UiDebugRequest) -> u64 {
    match request {
        UiDebugRequest::Evaluate { timeout_ms, .. } => {
            timeout_ms.unwrap_or(UI_DEBUG_DEFAULT_TIMEOUT_MS)
        }
        UiDebugRequest::Ping | UiDebugRequest::CaptureScreenshot { .. } => {
            UI_DEBUG_DEFAULT_TIMEOUT_MS
        }
    }
}
