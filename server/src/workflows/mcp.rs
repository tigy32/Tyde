use std::net::SocketAddr;

use axum::{Json, Router, response::IntoResponse, routing::get};
use protocol::{AgentId, WorkflowStepRunSnapshotStatus};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig,
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
    },
};
use serde::{Deserialize, Serialize};
use tokio::runtime;
use uuid::Uuid;

use crate::host::HostHandle;

pub(crate) const WORKFLOW_PROGRESS_MCP_SERVER_NAME: &str = "tyde-workflow-progress";
const WORKFLOW_AGENT_ID_HEADER: &str = "x-tyde-agent-id";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";

#[derive(Clone, Debug)]
pub(crate) struct WorkflowMcpHandle {
    pub(crate) url: String,
}

#[derive(Clone)]
struct TydeWorkflowMcpServer {
    host: HostHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeWorkflowMcpServer {
    fn new(host: HostHandle) -> Self {
        Self {
            host,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowReportStepToolInput {
    pub step_id: Option<String>,
    pub parent_step_id: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub agent_id: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowFinishToolInput {
    pub success: Option<bool>,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::json(value)?]))
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

#[tool_router]
impl TydeWorkflowMcpServer {
    #[tool(
        description = "Report or update a Tyde Workflow progress step for the calling coordinator agent."
    )]
    async fn tyde_workflow_report_step(
        &self,
        Parameters(input): Parameters<WorkflowReportStepToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller_agent_id = match request_agent_id_from_parts(&parts) {
            Ok(Some(agent_id)) => agent_id,
            Ok(None) => {
                return Ok(err_text(
                    "tyde_workflow_report_step requires calling agent_id",
                ));
            }
            Err(err) => return Ok(err_text(err)),
        };
        match self.host.workflow_report_step(caller_agent_id, input).await {
            Ok(snapshot) => ok_json(snapshot),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Finish the Tyde Workflow run for the calling coordinator agent.")]
    async fn tyde_workflow_finish(
        &self,
        Parameters(input): Parameters<WorkflowFinishToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller_agent_id = match request_agent_id_from_parts(&parts) {
            Ok(Some(agent_id)) => agent_id,
            Ok(None) => return Ok(err_text("tyde_workflow_finish requires calling agent_id")),
            Err(err) => return Ok(err_text(err)),
        };
        match self.host.workflow_finish(caller_agent_id, input).await {
            Ok(snapshot) => ok_json(snapshot),
            Err(err) => Ok(err_text(err)),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeWorkflowMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tyde Workflow progress tools. The host derives the workflow run from the calling agent; do not provide or invent run ids."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

pub(crate) fn start_server(
    bind_addr: Option<SocketAddr>,
    host_handle: HostHandle,
) -> Result<WorkflowMcpHandle, String> {
    let bind_addr = bind_addr.unwrap_or_else(|| {
        DEFAULT_BIND_ADDR
            .parse()
            .expect("default loopback workflow MCP bind addr must parse")
    });
    if !bind_addr.ip().is_loopback() {
        return Err(format!(
            "workflow MCP server must bind to loopback only, got {bind_addr}"
        ));
    }

    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind workflow MCP HTTP server on {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set workflow MCP listener nonblocking: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read workflow MCP listener addr: {err}"))?;

    std::thread::Builder::new()
        .name("tyde-workflow-mcp".to_string())
        .spawn(move || {
            let runtime = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build workflow MCP runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failed to create tokio workflow MCP listener");
                let mcp_service: StreamableHttpService<TydeWorkflowMcpServer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(TydeWorkflowMcpServer::new(host_handle.clone())),
                        Default::default(),
                        StreamableHttpServerConfig {
                            stateful_mode: false,
                            sse_keep_alive: None,
                            ..Default::default()
                        },
                    );
                let router = Router::new()
                    .route("/healthz", get(healthz_handler))
                    .nest_service("/mcp", mcp_service);
                if let Err(err) = axum::serve(listener, router).await {
                    tracing::warn!("workflow MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn workflow MCP server thread: {err}"))?;

    Ok(WorkflowMcpHandle {
        url: format!("http://{local_addr}/mcp"),
    })
}

pub(crate) fn parse_step_status(
    value: Option<&str>,
) -> Result<WorkflowStepRunSnapshotStatus, String> {
    match value.unwrap_or("running").trim() {
        "pending" => Ok(WorkflowStepRunSnapshotStatus::Pending),
        "running" => Ok(WorkflowStepRunSnapshotStatus::Running),
        "completed" | "complete" | "done" => Ok(WorkflowStepRunSnapshotStatus::Completed),
        "failed" | "error" => Ok(WorkflowStepRunSnapshotStatus::Failed),
        "cancelled" | "canceled" => Ok(WorkflowStepRunSnapshotStatus::Cancelled),
        other => Err(format!("unknown workflow step status {other:?}")),
    }
}

fn request_agent_id_from_parts(
    parts: &axum::http::request::Parts,
) -> Result<Option<AgentId>, String> {
    if let Some(agent_id) = parts
        .headers
        .get(WORKFLOW_AGENT_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return parse_agent_id(agent_id).map(Some);
    }

    let target = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| parts.uri.path());
    split_request_target(target).map(|(_, agent_id)| agent_id)
}

fn parse_agent_id(input: &str) -> Result<AgentId, String> {
    Uuid::parse_str(input).map_err(|err| format!("invalid agent_id '{input}': {err}"))?;
    Ok(AgentId(input.to_string()))
}

fn split_request_target(target: &str) -> Result<(&str, Option<AgentId>), String> {
    let path = target.split('?').next().unwrap_or(target);
    let Some((_, query)) = target.split_once('?') else {
        return Ok((path, None));
    };
    Ok((path, parse_agent_id_from_query(query)?))
}

fn parse_agent_id_from_query(query: &str) -> Result<Option<AgentId>, String> {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "agent_id" {
            continue;
        }
        let decoded = percent_decode_query_component(value)
            .ok_or_else(|| format!("invalid agent_id query parameter encoding: {value}"))?;
        let agent_id = parse_agent_id(&decoded)?;
        return Ok(Some(agent_id));
    }
    Ok(None)
}

fn percent_decode_query_component(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let decoded = (decode_hex_nibble(high)? << 4) | decode_hex_nibble(low)?;
                bytes.push(decoded);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn healthz_handler() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}
