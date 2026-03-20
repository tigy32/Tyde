use parking_lot::Mutex;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::LazyLock;

use axum::{response::IntoResponse, routing::get, Json, Router};
use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        tool::{Extension, Parameters},
    },
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
    await_agents_internal, cancel_agent_internal, create_workbench_internal,
    delete_workbench_internal, list_agents_internal, run_agent_internal,
    send_agent_message_internal, spawn_agent_internal, AgentIdRequest, AppState,
    AwaitAgentsRequest, SendAgentMessageRequest, SpawnAgentRequest,
};

const MCP_HTTP_BIND_ENV: &str = "TYDE_AGENT_MCP_HTTP_BIND_ADDR";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:47771";
const MCP_PATH: &str = "/mcp";

struct RunningMcpHttpServer {
    url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

static RUNNING_MCP_HTTP_SERVER: LazyLock<Mutex<Option<RunningMcpHttpServer>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone)]
struct TydeAgentMcpServer {
    app: tauri::AppHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeAgentMcpServer {
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
struct SpawnAgentToolInput {
    /// One or more workspace root directories the agent should operate in.
    workspace_roots: Vec<String>,
    /// The instruction/prompt for the agent.
    prompt: String,
    /// Backend to use (e.g. "tycode", "claude", "codex"). Defaults to "tycode".
    backend_kind: Option<String>,
    /// Parent agent ID if this is a sub-agent.
    parent_agent_id: Option<u64>,
    /// Human-readable name for the agent.
    name: String,
    /// Whether this is an ephemeral (non-persisted) session.
    ephemeral: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct RunAgentToolInput {
    /// One or more workspace root directories the agent should operate in.
    workspace_roots: Vec<String>,
    /// The instruction/prompt for the agent.
    prompt: String,
    /// Backend to use (e.g. "tycode", "claude", "codex"). Defaults to "tycode".
    backend_kind: Option<String>,
    /// Parent agent ID if this is a sub-agent.
    parent_agent_id: Option<u64>,
    /// Human-readable name for the agent.
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct SendAgentMessageToolInput {
    /// The agent to send the message to.
    agent_id: u64,
    /// The follow-up message text.
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct AwaitAgentsToolInput {
    /// Agent IDs to watch. If omitted, watches all running agents.
    agent_ids: Option<Vec<u64>>,
    /// Idle timeout in milliseconds. Resets on agent activity. Defaults to 60s.
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct AgentIdToolInput {
    /// The agent to cancel.
    agent_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct CreateWorkbenchToolInput {
    /// The parent workspace path (the git repo root).
    parent_workspace_path: String,
    /// The branch name for the new worktree (e.g. "feature-login").
    branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DeleteWorkbenchToolInput {
    /// The workspace path of the workbench to delete.
    workspace_path: String,
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

fn spawn_request_from(input: SpawnAgentToolInput) -> SpawnAgentRequest {
    SpawnAgentRequest {
        workspace_roots: input.workspace_roots,
        prompt: input.prompt,
        backend_kind: input.backend_kind,
        parent_agent_id: input.parent_agent_id,
        name: input.name,
        ephemeral: input.ephemeral,
        images: None,
    }
}

/// Extract the caller's agent ID from the `X-Tyde-Agent-Id` HTTP header.
/// This header is set by `startup_mcp_servers_for_agent` when the MCP server
/// config is injected into each agent's session.
fn caller_agent_id_from_parts(parts: &http::request::Parts) -> Option<u64> {
    let header = parts.headers.get("x-tyde-agent-id")?;
    let s = header.to_str().ok()?;
    s.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// MCP Tools (7 tools — agent orchestration + workbench management)
// ---------------------------------------------------------------------------

#[tool_router]
impl TydeAgentMcpServer {
    #[tool(
        description = "Spawn a Tyde agent and return immediately with its agent_id. Use this when you want to launch multiple agents in parallel and then wait for them with tyde_await_agent. For the common case of spawning a single agent and waiting for its result, prefer tyde_run_agent instead."
    )]
    async fn tyde_spawn_agent(
        &self,
        Parameters(input): Parameters<SpawnAgentToolInput>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let mut request = spawn_request_from(input);
        // Use the caller's agent ID from the HTTP header as parent, unless
        // the caller explicitly provided a parent_agent_id in the request.
        if request.parent_agent_id.is_none() {
            request.parent_agent_id = caller_agent_id_from_parts(&parts);
        }
        match spawn_agent_internal(&self.app, app_state.inner(), request).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Spawn a Tyde agent and block until it finishes. Returns the agent's final message and any error. This is the simplest way to run an agent — one call does everything."
    )]
    async fn tyde_run_agent(
        &self,
        Parameters(input): Parameters<RunAgentToolInput>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let explicit_parent = input.parent_agent_id;
        let mut request = SpawnAgentRequest {
            workspace_roots: input.workspace_roots,
            prompt: input.prompt,
            backend_kind: input.backend_kind,
            parent_agent_id: explicit_parent,
            name: input.name,
            ephemeral: Some(false),
            images: None,
        };
        if request.parent_agent_id.is_none() {
            request.parent_agent_id = caller_agent_id_from_parts(&parts);
        }
        match run_agent_internal(&self.app, app_state.inner(), request).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Block until one or more agents stop running. Returns the stopped agents with their messages and a list of still-running agent IDs. If agent_ids is omitted, watches all running agents. Use this after spawning multiple agents with tyde_spawn_agent to wait for any of them to finish — like epoll."
    )]
    async fn tyde_await_agent(
        &self,
        Parameters(input): Parameters<AwaitAgentsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let request = AwaitAgentsRequest {
            agent_ids: input.agent_ids,
            timeout_ms: input.timeout_ms,
        };
        match await_agents_internal(app_state.inner(), request).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Send a follow-up message to an existing Tyde agent.")]
    async fn tyde_send_agent_message(
        &self,
        Parameters(input): Parameters<SendAgentMessageToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let request = SendAgentMessageRequest {
            agent_id: input.agent_id,
            message: input.message,
        };
        match send_agent_message_internal(&self.app, app_state.inner(), request).await {
            Ok(()) => ok_json(serde_json::json!({ "ok": true })),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Cancel a running Tyde agent. Interrupts it and shuts down its subprocess. Returns the agent's final message."
    )]
    async fn tyde_cancel_agent(
        &self,
        Parameters(input): Parameters<AgentIdToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        let request = AgentIdRequest {
            agent_id: input.agent_id,
        };
        match cancel_agent_internal(&self.app, app_state.inner(), request).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "List all Tyde agents with their running state, last message, and metadata."
    )]
    async fn tyde_list_agents(&self) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match list_agents_internal(app_state.inner()).await {
            Ok(value) => ok_json(value),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Create a git worktree workbench from a parent workspace. Runs `git worktree add -b <branch> <path>` and registers the new workspace in the Tyde project list. Returns the new workspace path, which can be passed directly to tyde_spawn_agent's workspace_roots."
    )]
    async fn tyde_create_workbench(
        &self,
        Parameters(input): Parameters<CreateWorkbenchToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match create_workbench_internal(
            &self.app,
            app_state.inner(),
            input.parent_workspace_path,
            input.branch,
        )
        .await
        {
            Ok(workspace_path) => ok_json(serde_json::json!({ "workspace_path": workspace_path })),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Delete a git worktree workbench. Closes all conversations, removes the workspace view, runs `git worktree remove`, and unregisters the workspace from the Tyde project list."
    )]
    async fn tyde_delete_workbench(
        &self,
        Parameters(input): Parameters<DeleteWorkbenchToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let app_state = self.app.state::<AppState>();
        match delete_workbench_internal(&self.app, app_state.inner(), input.workspace_path).await {
            Ok(()) => ok_json(serde_json::json!({ "ok": true })),
            Err(err) => Ok(err_text(err)),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeAgentMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for orchestrating Tyde coding agents. Use tyde_run_agent for simple tasks (spawn + wait + result in one call). Use tyde_spawn_agent + tyde_await_agent for parallel agent orchestration."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP server lifecycle (unchanged)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub(crate) fn start_agent_mcp_http_server(app: &tauri::AppHandle) -> Result<(), String> {
    let mut guard = RUNNING_MCP_HTTP_SERVER.lock();
    if guard.is_some() {
        return Ok(());
    }

    let bind_addr = resolve_bind_addr();
    let listener = match std::net::TcpListener::bind(bind_addr) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(
                "Agent MCP HTTP server failed to bind {bind_addr}: {err}; retrying on ephemeral loopback port"
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
    tracing::info!("Agent MCP HTTP server listening at {public_url}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    *guard = Some(RunningMcpHttpServer {
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
                tracing::warn!("Agent MCP HTTP server failed to create async listener: {err}");
                clear_running_server_if_url(&cleanup_url);
                return;
            }
        };

        let mcp_service: StreamableHttpService<TydeAgentMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(TydeAgentMcpServer::new(app_handle.clone())),
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
            tracing::warn!("Agent MCP HTTP server stopped: {err}");
        }
        clear_running_server_if_url(&cleanup_url);
    });
    Ok(())
}

pub(crate) fn stop_agent_mcp_http_server() -> bool {
    let running = RUNNING_MCP_HTTP_SERVER.lock().take();

    let Some(mut running) = running else {
        return false;
    };

    if let Some(tx) = running.shutdown_tx.take() {
        let _ = tx.send(());
    }
    true
}

pub(crate) fn is_agent_mcp_http_server_running() -> bool {
    RUNNING_MCP_HTTP_SERVER.lock().is_some()
}

pub(crate) fn agent_mcp_http_server_url() -> Option<String> {
    RUNNING_MCP_HTTP_SERVER
        .lock()
        .as_ref()
        .map(|running| running.url.clone())
}

fn clear_running_server_if_url(url: &str) {
    let mut guard = RUNNING_MCP_HTTP_SERVER.lock();
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
        std::env::var(MCP_HTTP_BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    resolve_bind_addr_value(&requested)
}

fn resolve_bind_addr_value(requested: &str) -> SocketAddr {
    match requested.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => addr,
        Ok(addr) => {
            tracing::warn!(
                "Ignoring non-loopback {MCP_HTTP_BIND_ENV}={addr}; falling back to {DEFAULT_BIND_ADDR}"
            );
            DEFAULT_BIND_ADDR
                .parse()
                .expect("DEFAULT_BIND_ADDR must be a valid socket address")
        }
        Err(err) => {
            tracing::warn!(
                "Invalid {MCP_HTTP_BIND_ENV}='{requested}': {err}; falling back to {DEFAULT_BIND_ADDR}"
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
        let addr = resolve_bind_addr_value("127.0.0.1:5000");
        assert_eq!(addr, "127.0.0.1:5000".parse().expect("valid socket addr"));
    }

    #[test]
    fn resolve_bind_addr_rejects_non_loopback() {
        let addr = resolve_bind_addr_value("0.0.0.0:5000");
        assert_eq!(
            addr,
            DEFAULT_BIND_ADDR
                .parse()
                .expect("default bind address should parse")
        );
    }
}
