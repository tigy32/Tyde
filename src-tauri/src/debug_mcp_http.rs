use parking_lot::Mutex;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::LazyLock;

use axum::{response::IntoResponse, routing::get, Json, Router};
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::{
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
        StreamableHttpServerConfig,
    },
    Error as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

use crate::{
    debug_events_since_internal, debug_snapshot_internal, debug_ui_action_internal, AppState,
    DebugEventsSinceRequest,
};

const DEBUG_MCP_HTTP_BIND_ENV: &str = "TYDE_DEBUG_MCP_HTTP_BIND_ADDR";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:47772";
const MCP_PATH: &str = "/mcp";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

struct RunningDebugMcpHttpServer {
    url: String,
    /// Dropping this sender signals graceful shutdown to the server task.
    #[allow(dead_code)]
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

static RUNNING_DEBUG_MCP_HTTP_SERVER: LazyLock<Mutex<Option<RunningDebugMcpHttpServer>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone)]
struct TydeDebugMcpServer {
    app: tauri::AppHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeDebugMcpServer {
    fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DebugEventsToolInput {
    since_seq: Option<u64>,
    limit: Option<usize>,
    stream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct QueryElementsToolInput {
    selector: String,
    include_text: Option<bool>,
    include_html: Option<bool>,
    max_nodes: Option<usize>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct SelectorIndexToolInput {
    selector: String,
    index: Option<usize>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct GetTextToolInput {
    selector: String,
    index: Option<usize>,
    max_length: Option<usize>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct ListTestIdsToolInput {
    pattern: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct CaptureScreenshotToolInput {
    selector: Option<String>,
    index: Option<usize>,
    max_dimension: Option<u32>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct TypeToolInput {
    selector: String,
    text: String,
    index: Option<usize>,
    append: Option<bool>,
    submit: Option<bool>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct KeyPressToolInput {
    key: String,
    code: Option<String>,
    ctrl: Option<bool>,
    alt: Option<bool>,
    shift: Option<bool>,
    meta: Option<bool>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct ScrollToolInput {
    selector: Option<String>,
    index: Option<usize>,
    dx: Option<f64>,
    dy: Option<f64>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct WaitForToolInput {
    selector: String,
    index: Option<usize>,
    state: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct EvaluateToolInput {
    /// JavaScript expression to evaluate in the webview. The body of an async
    /// function — use `return` to produce a value. Has access to the full DOM
    /// and any globals the app exposes.
    expression: String,
    timeout_ms: Option<u64>,
}

fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::json(value)?]))
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

pub(crate) fn validate_png_base64(data: &str) -> Result<(), String> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err("Screenshot response missing image data".to_string());
    }

    let decoded = BASE64_STANDARD
        .decode(trimmed)
        .map_err(|_| "Screenshot response contained invalid base64 image data".to_string())?;
    if decoded.len() < PNG_SIGNATURE.len() || !decoded.starts_with(&PNG_SIGNATURE) {
        return Err("Screenshot response image payload is not a PNG".to_string());
    }

    Ok(())
}

pub(crate) fn extract_valid_png_data(value: &serde_json::Value) -> Result<&str, String> {
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .ok_or_else(|| "Screenshot response missing data".to_string())?;
    let mime_type = value
        .get("mime_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("image/png");
    if mime_type != "image/png" {
        return Err(format!(
            "Screenshot encoding failed: expected image/png but got {mime_type}"
        ));
    }
    validate_png_base64(data)?;
    Ok(data)
}

async fn call_ui_action(
    app: &tauri::AppHandle,
    action: &str,
    params: serde_json::Value,
    timeout_ms: Option<u64>,
) -> Result<serde_json::Value, String> {
    let app_state = app.state::<AppState>();
    debug_ui_action_internal(app, app_state.inner(), action, params, timeout_ms).await
}

#[tool_router]
impl TydeDebugMcpServer {
    #[tool(description = "Get a runtime snapshot of Tyde app state useful for debugging.")]
    async fn tyde_debug_snapshot(&self) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match debug_snapshot_internal(app_state.inner()).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Read Tyde debug event log entries after a sequence number.")]
    async fn tyde_debug_events_since(
        &self,
        Parameters(input): Parameters<DebugEventsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let request = DebugEventsSinceRequest {
            since_seq: input.since_seq,
            limit: input.limit,
            stream: input.stream,
        };
        match debug_events_since_internal(app_state.inner(), request).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Query DOM elements in the Tyde UI by CSS selector.")]
    async fn tyde_debug_query_elements(
        &self,
        Parameters(input): Parameters<QueryElementsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "include_text": input.include_text,
            "include_html": input.include_html,
            "max_nodes": input.max_nodes,
        });
        match call_ui_action(&self.app, "query_elements", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Get text from the first (or indexed) UI element matching selector.")]
    async fn tyde_debug_get_text(
        &self,
        Parameters(input): Parameters<GetTextToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "max_length": input.max_length,
        });
        match call_ui_action(&self.app, "get_text", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "List active data-testid values in the Tyde UI.")]
    async fn tyde_debug_list_testids(
        &self,
        Parameters(input): Parameters<ListTestIdsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "pattern": input.pattern,
        });
        match call_ui_action(&self.app, "list_testids", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Capture a PNG screenshot of Tyde UI (optionally by selector).")]
    async fn tyde_debug_capture_screenshot(
        &self,
        Parameters(input): Parameters<CaptureScreenshotToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let timeout_ms = Some(input.timeout_ms.unwrap_or(30_000));
        let params = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "max_dimension": input.max_dimension,
            "timeout_ms": timeout_ms,
        });
        let value = match call_ui_action(&self.app, "capture_screenshot", params, timeout_ms).await
        {
            Ok(value) => value,
            Err(err) => return Ok(err_text(err)),
        };

        let data = match extract_valid_png_data(&value) {
            Ok(data) => data.to_string(),
            Err(err) => return Ok(err_text(err)),
        };

        let out = vec![
            Content::image(data, "image/png".to_string()),
            Content::json(value)?,
        ];
        Ok(CallToolResult::success(out))
    }

    #[tool(description = "Click a UI element in Tyde by CSS selector.")]
    async fn tyde_debug_click(
        &self,
        Parameters(input): Parameters<SelectorIndexToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
        });
        match call_ui_action(&self.app, "click", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Type text into a Tyde UI element by CSS selector.")]
    async fn tyde_debug_type(
        &self,
        Parameters(input): Parameters<TypeToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "text": input.text,
            "index": input.index,
            "append": input.append,
            "submit": input.submit,
        });
        match call_ui_action(&self.app, "type", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Dispatch a keyboard event in the Tyde UI.")]
    async fn tyde_debug_keypress(
        &self,
        Parameters(input): Parameters<KeyPressToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "key": input.key,
            "code": input.code,
            "ctrl": input.ctrl,
            "alt": input.alt,
            "shift": input.shift,
            "meta": input.meta,
        });
        match call_ui_action(&self.app, "keypress", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Scroll a Tyde UI element (or window if selector omitted).")]
    async fn tyde_debug_scroll(
        &self,
        Parameters(input): Parameters<ScrollToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "dx": input.dx,
            "dy": input.dy,
        });
        match call_ui_action(&self.app, "scroll", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Wait for a selector condition in Tyde UI.")]
    async fn tyde_debug_wait_for(
        &self,
        Parameters(input): Parameters<WaitForToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "state": input.state,
            "timeout_ms": input.timeout_ms,
        });
        match call_ui_action(&self.app, "wait_for", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Evaluate a JavaScript expression in the Tyde webview and return the result. The expression is the body of an async function — use `return` to produce a value. Has access to the full DOM and any globals the app exposes (e.g. window.__TYDE_BRIDGE__)."
    )]
    async fn tyde_debug_evaluate(
        &self,
        Parameters(input): Parameters<EvaluateToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "expression": input.expression,
        });
        match call_ui_action(&self.app, "evaluate", params, input.timeout_ms).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeDebugMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for debugging Tyde itself: inspect UI, capture screenshots, inspect event logs, and drive UI actions. All tools operate on the local Tyde instance."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub(crate) fn start_debug_mcp_http_server(app: &tauri::AppHandle) -> Result<(), String> {
    let mut guard = RUNNING_DEBUG_MCP_HTTP_SERVER.lock();
    if guard.is_some() {
        return Ok(());
    }

    let bind_addr = resolve_bind_addr();
    let listener = match std::net::TcpListener::bind(bind_addr) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(
                "Debug MCP HTTP server failed to bind {bind_addr}: {err}; retrying on ephemeral loopback port"
            );
            match std::net::TcpListener::bind("127.0.0.1:0") {
                Ok(listener) => listener,
                Err(fallback_err) => {
                    return Err(format!(
                        "failed to bind {bind_addr} ({err}); fallback bind failed: {fallback_err}"
                    ));
                }
            }
        }
    };

    if let Err(err) = listener.set_nonblocking(true) {
        return Err(format!("failed to set nonblocking listener: {err}"));
    }

    let local_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => return Err(format!("failed to resolve local listener address: {err}")),
    };

    let public_url = format!("http://{local_addr}{MCP_PATH}");
    tracing::info!("Debug MCP HTTP server listening at {public_url}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    *guard = Some(RunningDebugMcpHttpServer {
        url: public_url.clone(),
        shutdown_tx: Some(shutdown_tx),
    });
    drop(guard);

    let cleanup_url = public_url.clone();
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!("Debug MCP HTTP server failed to create async listener: {err}");
                clear_running_server_if_url(&cleanup_url);
                return;
            }
        };

        let mcp_service: StreamableHttpService<TydeDebugMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(TydeDebugMcpServer::new(app_handle.clone())),
                Default::default(),
                StreamableHttpServerConfig {
                    stateful_mode: true,
                    sse_keep_alive: None,
                },
            );

        let router = Router::new()
            .route("/healthz", get(healthz_handler))
            .nest_service(MCP_PATH, mcp_service);

        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(err) = server.await {
            tracing::warn!("Debug MCP HTTP server stopped: {err}");
        }
        clear_running_server_if_url(&cleanup_url);
    });
    Ok(())
}


fn clear_running_server_if_url(url: &str) {
    let mut guard = RUNNING_DEBUG_MCP_HTTP_SERVER.lock();
    let should_clear = guard.as_ref().map(|running| running.url.as_str()) == Some(url);
    if should_clear {
        guard.take();
    }
}

async fn healthz_handler() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

fn resolve_bind_addr() -> SocketAddr {
    let requested =
        std::env::var(DEBUG_MCP_HTTP_BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    resolve_bind_addr_value(&requested)
}

fn resolve_bind_addr_value(requested: &str) -> SocketAddr {
    match requested.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => addr,
        Ok(addr) => {
            tracing::warn!(
                "Ignoring non-loopback {DEBUG_MCP_HTTP_BIND_ENV}={addr}; falling back to {DEFAULT_BIND_ADDR}"
            );
            DEFAULT_BIND_ADDR
                .parse()
                .expect("DEFAULT_BIND_ADDR must be a valid socket address")
        }
        Err(err) => {
            tracing::warn!(
                "Invalid {DEBUG_MCP_HTTP_BIND_ENV}='{requested}': {err}; falling back to {DEFAULT_BIND_ADDR}"
            );
            DEFAULT_BIND_ADDR
                .parse()
                .expect("DEFAULT_BIND_ADDR must be a valid socket address")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bind_addr_accepts_loopback() {
        let addr = resolve_bind_addr_value("127.0.0.1:5001");
        assert_eq!(addr, "127.0.0.1:5001".parse().expect("valid socket addr"));
    }

    #[test]
    fn resolve_bind_addr_rejects_non_loopback() {
        let addr = resolve_bind_addr_value("0.0.0.0:5001");
        assert_eq!(
            addr,
            DEFAULT_BIND_ADDR
                .parse()
                .expect("default bind address should parse")
        );
    }

    #[test]
    fn validate_png_base64_accepts_valid_png() {
        // 1x1 transparent PNG
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO7m6i0AAAAASUVORK5CYII=";
        assert!(validate_png_base64(png).is_ok());
    }

    #[test]
    fn validate_png_base64_rejects_invalid_base64() {
        let err = validate_png_base64("not-base64!").expect_err("expected invalid base64");
        assert!(err.contains("invalid base64"));
    }

    #[test]
    fn validate_png_base64_rejects_non_png_payload() {
        let err = validate_png_base64("aGVsbG8=").expect_err("expected non-png to be rejected");
        assert!(err.contains("not a PNG"));
    }
}
