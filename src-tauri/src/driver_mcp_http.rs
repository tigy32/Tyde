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

use crate::{
    debug_mcp_http::extract_valid_png_data,
    dev_instance, AppState,
};

const DRIVER_MCP_HTTP_BIND_ENV: &str = "TYDE_DRIVER_MCP_HTTP_BIND_ADDR";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:47773";
const MCP_PATH: &str = "/mcp";

struct RunningDriverMcpHttpServer {
    url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

static RUNNING_DRIVER_MCP_HTTP_SERVER: LazyLock<Mutex<Option<RunningDriverMcpHttpServer>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone)]
struct TydeDriverMcpServer {
    app: tauri::AppHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeDriverMcpServer {
    fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool input types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DevInstanceStartToolInput {
    /// Path to the Tyde project root directory to build and run.
    project_dir: String,
}

/// Empty input — no parameters needed.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct EmptyInput {}

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::json(value)?]))
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

/// Proxy a tool call to the dev instance's debug MCP server.
/// Returns an error CallToolResult if no dev instance is running.
async fn proxy_tool(
    app: &tauri::AppHandle,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<CallToolResult, McpError> {
    let app_state = app.state::<AppState>();
    match dev_instance::proxy_debug_tool_call(app_state.inner(), tool_name, arguments).await {
        Ok(value) => match serde_json::from_value::<CallToolResult>(value.clone()) {
            Ok(result) => Ok(result),
            Err(_) => ok_json(value),
        },
        Err(err) => Ok(err_text(err)),
    }
}

// ---------------------------------------------------------------------------
// MCP Tools — 13 tools: 2 dev instance lifecycle + 11 proxied debug tools
// ---------------------------------------------------------------------------

#[tool_router]
impl TydeDriverMcpServer {
    // -- Dev instance lifecycle ------------------------------------------------

    #[tool(
        description = "Build and launch a Tyde dev instance with hot-reload. Runs `npx tauri dev` in the given project directory, waits for the debug MCP server to become ready, and returns the debug MCP URL. Only one dev instance can run at a time. The tyde_debug_* tools on this server will target the launched instance. This may take several minutes on first build."
    )]
    async fn tyde_dev_instance_start(
        &self,
        Parameters(input): Parameters<DevInstanceStartToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match dev_instance::start_dev_instance(app_state.inner(), input.project_dir).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Stop the running Tyde dev instance. Kills the dev process and cleans up. The tyde_debug_* tools will return errors until a new instance is started."
    )]
    async fn tyde_dev_instance_stop(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match dev_instance::stop_dev_instance(app_state.inner()).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    // -- Proxied debug tools ---------------------------------------------------

    #[tool(description = "Get a runtime snapshot of the dev instance's app state.")]
    async fn tyde_debug_snapshot(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        proxy_tool(&self.app, "tyde_debug_snapshot", serde_json::json!({})).await
    }

    #[tool(description = "Read debug event log entries from the dev instance.")]
    async fn tyde_debug_events_since(
        &self,
        Parameters(input): Parameters<DebugEventsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "since_seq": input.since_seq,
            "limit": input.limit,
            "stream": input.stream,
        });
        proxy_tool(&self.app, "tyde_debug_events_since", args).await
    }

    #[tool(description = "Query DOM elements in the dev instance by CSS selector.")]
    async fn tyde_debug_query_elements(
        &self,
        Parameters(input): Parameters<QueryElementsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "include_text": input.include_text,
            "include_html": input.include_html,
            "max_nodes": input.max_nodes,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_query_elements", args).await
    }

    #[tool(description = "Get text from a UI element in the dev instance.")]
    async fn tyde_debug_get_text(
        &self,
        Parameters(input): Parameters<GetTextToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "max_length": input.max_length,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_get_text", args).await
    }

    #[tool(description = "List active data-testid values in the dev instance.")]
    async fn tyde_debug_list_testids(
        &self,
        Parameters(input): Parameters<ListTestIdsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "pattern": input.pattern,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_list_testids", args).await
    }

    #[tool(description = "Capture a PNG screenshot of the dev instance (optionally by selector).")]
    async fn tyde_debug_capture_screenshot(
        &self,
        Parameters(input): Parameters<CaptureScreenshotToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "max_dimension": input.max_dimension,
            "timeout_ms": input.timeout_ms,
        });

        let app_state = self.app.state::<AppState>();
        let value = match dev_instance::proxy_debug_tool_call(
            app_state.inner(),
            "tyde_debug_capture_screenshot",
            args,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => return Ok(err_text(err)),
        };

        // The proxy returns the raw CallToolResult JSON. Extract the screenshot
        // content so we can return a proper Content::image to the client.
        // The dev instance's debug MCP returns content[1] as the JSON metadata.
        let content_arr = value.get("content").and_then(|c| c.as_array());
        if let Some(items) = content_arr {
            // Look for the JSON metadata item that has `data` and `mime_type` fields.
            for item in items {
                if let Some(json_text) = item.get("text").and_then(|t| t.as_str()) {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_text) {
                        if let Ok(data) = extract_valid_png_data(&meta) {
                            let out = vec![
                                Content::image(data.to_string(), "image/png".to_string()),
                                Content::json(meta)?,
                            ];
                            return Ok(CallToolResult::success(out));
                        }
                    }
                }
            }
        }

        // If we couldn't extract the image, return the raw proxy result.
        match serde_json::from_value::<CallToolResult>(value.clone()) {
            Ok(result) => Ok(result),
            Err(_) => ok_json(value),
        }
    }

    #[tool(description = "Click a UI element in the dev instance by CSS selector.")]
    async fn tyde_debug_click(
        &self,
        Parameters(input): Parameters<SelectorIndexToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_click", args).await
    }

    #[tool(description = "Type text into a UI element in the dev instance.")]
    async fn tyde_debug_type(
        &self,
        Parameters(input): Parameters<TypeToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "text": input.text,
            "index": input.index,
            "append": input.append,
            "submit": input.submit,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_type", args).await
    }

    #[tool(description = "Dispatch a keyboard event in the dev instance.")]
    async fn tyde_debug_keypress(
        &self,
        Parameters(input): Parameters<KeyPressToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "key": input.key,
            "code": input.code,
            "ctrl": input.ctrl,
            "alt": input.alt,
            "shift": input.shift,
            "meta": input.meta,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_keypress", args).await
    }

    #[tool(description = "Scroll a UI element (or window) in the dev instance.")]
    async fn tyde_debug_scroll(
        &self,
        Parameters(input): Parameters<ScrollToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "dx": input.dx,
            "dy": input.dy,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_scroll", args).await
    }

    #[tool(description = "Wait for a selector condition in the dev instance.")]
    async fn tyde_debug_wait_for(
        &self,
        Parameters(input): Parameters<WaitForToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "selector": input.selector,
            "index": input.index,
            "state": input.state,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_wait_for", args).await
    }
}

#[tool_handler]
impl ServerHandler for TydeDriverMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for spawning and controlling a Tyde dev instance. Use tyde_dev_instance_start to build and launch a dev instance, then use the tyde_debug_* tools to interact with its UI. All debug tools proxy to the dev instance — they never target the host."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP server lifecycle
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub(crate) fn start_driver_mcp_http_server(app: &tauri::AppHandle) -> Result<(), String> {
    let mut guard = RUNNING_DRIVER_MCP_HTTP_SERVER.lock();
    if guard.is_some() {
        return Ok(());
    }

    let bind_addr = resolve_bind_addr();
    let listener = match std::net::TcpListener::bind(bind_addr) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(
                "Driver MCP HTTP server failed to bind {bind_addr}: {err}; retrying on ephemeral loopback port"
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
    tracing::info!("Driver MCP HTTP server listening at {public_url}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    *guard = Some(RunningDriverMcpHttpServer {
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
                tracing::warn!("Driver MCP HTTP server failed to create async listener: {err}");
                clear_running_server_if_url(&cleanup_url);
                return;
            }
        };

        let mcp_service: StreamableHttpService<TydeDriverMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(TydeDriverMcpServer::new(app_handle.clone())),
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
            tracing::warn!("Driver MCP HTTP server stopped: {err}");
        }
        clear_running_server_if_url(&cleanup_url);
    });
    Ok(())
}

pub(crate) fn stop_driver_mcp_http_server() -> bool {
    let running = RUNNING_DRIVER_MCP_HTTP_SERVER.lock().take();

    let Some(mut running) = running else {
        return false;
    };

    if let Some(tx) = running.shutdown_tx.take() {
        let _ = tx.send(());
    }
    true
}

pub(crate) fn is_driver_mcp_http_server_running() -> bool {
    RUNNING_DRIVER_MCP_HTTP_SERVER.lock().is_some()
}

pub(crate) fn driver_mcp_http_server_url() -> Option<String> {
    RUNNING_DRIVER_MCP_HTTP_SERVER
        .lock()
        .as_ref()
        .map(|running| running.url.clone())
}

fn clear_running_server_if_url(url: &str) {
    let mut guard = RUNNING_DRIVER_MCP_HTTP_SERVER.lock();
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
        std::env::var(DRIVER_MCP_HTTP_BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    resolve_bind_addr_value(&requested)
}

fn resolve_bind_addr_value(requested: &str) -> SocketAddr {
    match requested.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => addr,
        Ok(addr) => {
            tracing::warn!(
                "Ignoring non-loopback {DRIVER_MCP_HTTP_BIND_ENV}={addr}; falling back to {DEFAULT_BIND_ADDR}"
            );
            DEFAULT_BIND_ADDR
                .parse()
                .expect("DEFAULT_BIND_ADDR must be a valid socket address")
        }
        Err(err) => {
            tracing::warn!(
                "Invalid {DRIVER_MCP_HTTP_BIND_ENV}='{requested}': {err}; falling back to {DEFAULT_BIND_ADDR}"
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
        let addr = resolve_bind_addr_value("127.0.0.1:5002");
        assert_eq!(addr, "127.0.0.1:5002".parse().expect("valid socket addr"));
    }

    #[test]
    fn resolve_bind_addr_rejects_non_loopback() {
        let addr = resolve_bind_addr_value("0.0.0.0:5002");
        assert_eq!(
            addr,
            DEFAULT_BIND_ADDR
                .parse()
                .expect("default bind address should parse")
        );
    }
}
