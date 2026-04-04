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

use crate::{dev_instance, run_query_screenshot_agent, AppState};

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
    /// Optional workspace directory to open automatically on startup.
    /// Bypasses the native file dialog so MCP clients can control the
    /// full lifecycle without manual intervention.
    workspace_path: Option<String>,
    /// Optional SSH host to spawn the dev instance on (e.g. "user@devbox").
    /// If omitted, the instance runs locally.
    ssh_host: Option<String>,
    /// Optional agent ID to bind this instance to. When the agent terminates,
    /// its dev instance is automatically stopped.
    agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DevInstanceStopToolInput {
    /// ID of the dev instance to stop. If omitted and exactly one instance
    /// is running, that instance is stopped.
    instance_id: Option<u64>,
}

/// Empty input — no parameters needed.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct EmptyInput {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct SnapshotToolInput {
    /// Target dev instance. Required when multiple instances are running.
    instance_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DebugEventsToolInput {
    /// Target dev instance. Required when multiple instances are running.
    instance_id: Option<u64>,
    since_seq: Option<u64>,
    limit: Option<usize>,
    stream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct EvaluateToolInput {
    /// Target dev instance. Required when multiple instances are running.
    instance_id: Option<u64>,
    /// JavaScript expression to evaluate in the webview. The body of an async
    /// function — use `return` to produce a value. Has access to the full DOM
    /// and any globals the app exposes.
    expression: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct QueryScreenshotToolInput {
    /// Target dev instance. Required when multiple instances are running.
    instance_id: Option<u64>,
    /// A visual question about the UI, e.g. "Is the sidebar collapsed or expanded?",
    /// "What color is the error banner?", "Does the layout look correct?"
    question: String,
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

/// Proxy a tool call to a dev instance's debug MCP server.
async fn proxy_tool(
    app: &tauri::AppHandle,
    tool_name: &str,
    instance_id: Option<u64>,
    arguments: serde_json::Value,
) -> Result<CallToolResult, McpError> {
    let app_state = app.state::<AppState>();
    match dev_instance::proxy_debug_tool_call(app_state.inner(), instance_id, tool_name, arguments)
        .await
    {
        Ok(value) => match serde_json::from_value::<CallToolResult>(value.clone()) {
            Ok(result) => Ok(result),
            Err(_) => ok_json(value),
        },
        Err(err) => Ok(err_text(err)),
    }
}

// ---------------------------------------------------------------------------
// MCP Tools — 7 tools: 3 dev instance lifecycle + 3 proxied debug tools + 1 query_screenshot
// ---------------------------------------------------------------------------

#[tool_router]
impl TydeDriverMcpServer {
    // -- Dev instance lifecycle ------------------------------------------------

    #[tool(
        description = "Build and launch a Tyde dev instance with hot-reload. Runs `npx tauri dev` in the given project directory, waits for the debug MCP server to become ready, and returns the debug MCP URL and instance ID. Supports multiple concurrent instances (local or on SSH remote hosts). The tyde_debug_* tools on this server will target the launched instance. This may take several minutes on first build."
    )]
    async fn tyde_dev_instance_start(
        &self,
        Parameters(input): Parameters<DevInstanceStartToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match dev_instance::start_dev_instance(
            app_state.inner(),
            input.project_dir,
            input.workspace_path,
            input.ssh_host,
            input.agent_id,
        )
        .await
        {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Stop a running Tyde dev instance. Kills the dev process and cleans up. If instance_id is omitted and exactly one instance is running, that instance is stopped. The tyde_debug_* tools will return errors for this instance after stopping."
    )]
    async fn tyde_dev_instance_stop(
        &self,
        Parameters(input): Parameters<DevInstanceStopToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let instance_id =
            match dev_instance::resolve_instance_id(app_state.inner(), input.instance_id) {
                Ok(id) => id,
                Err(err) => return Ok(err_text(err)),
            };
        match dev_instance::stop_dev_instance(app_state.inner(), instance_id).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "List all running dev instances with their IDs, project directories, SSH hosts, and bound agent IDs."
    )]
    async fn tyde_dev_instance_list(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let list = dev_instance::dev_instance_list(app_state.inner());
        ok_json(list)
    }

    // -- Proxied debug tools ---------------------------------------------------

    #[tool(description = "Get a runtime snapshot of the dev instance's app state.")]
    async fn tyde_debug_snapshot(
        &self,
        Parameters(input): Parameters<SnapshotToolInput>,
    ) -> Result<CallToolResult, McpError> {
        proxy_tool(
            &self.app,
            "tyde_debug_snapshot",
            input.instance_id,
            serde_json::json!({}),
        )
        .await
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
        proxy_tool(
            &self.app,
            "tyde_debug_events_since",
            input.instance_id,
            args,
        )
        .await
    }

    #[tool(
        description = "Evaluate a JavaScript expression in the Tyde webview and return the result. The expression is the body of an async function — use `return` to produce a value. Has access to the full DOM and any globals the app exposes (e.g. window.__TYDE_BRIDGE__). Use this for all DOM interaction: querying elements, reading text, clicking, typing, dispatching events, scrolling, waiting for conditions, etc."
    )]
    async fn tyde_debug_evaluate(
        &self,
        Parameters(input): Parameters<EvaluateToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::json!({
            "expression": input.expression,
            "timeout_ms": input.timeout_ms,
        });
        proxy_tool(&self.app, "tyde_debug_evaluate", input.instance_id, args).await
    }

    // -- High-level screenshot query -------------------------------------------

    #[tool(
        description = "Ask a visual question about the dev instance UI by taking a screenshot and having an agent describe what it sees. Use this for visual/styling validation — layout, colors, spacing, visual state. For DOM-level questions (text content, element presence, attributes), prefer tyde_debug_evaluate instead."
    )]
    async fn tyde_debug_query_screenshot(
        &self,
        Parameters(input): Parameters<QueryScreenshotToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match run_query_screenshot_agent(
            &self.app,
            app_state.inner(),
            input.instance_id,
            input.question,
        )
        .await
        {
            Ok(answer) => Ok(CallToolResult::success(vec![Content::text(answer)])),
            Err(err) => Ok(err_text(err)),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeDriverMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for spawning and controlling Tyde dev instances. Supports multiple concurrent instances (local or on SSH remote hosts). Use tyde_dev_instance_start to launch, tyde_dev_instance_list to see running instances, and tyde_debug_* tools to interact with a specific instance's UI. When multiple instances are running, pass instance_id to target the correct one."
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
