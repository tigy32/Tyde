use std::net::SocketAddr;

use axum::{Json, Router, response::IntoResponse, routing::get};
use protocol::{
    AgentId, ReviewErrorCode, ReviewId, ReviewLocation, ReviewSeverity, ReviewSuggestionId,
};
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
use crate::review::reviewer::{ProposeReviewCommentArgs, ReviewerToolBridge};

pub(crate) const REVIEW_FEEDBACK_MCP_SERVER_NAME: &str = "tyde-review-feedback";
const REVIEW_FEEDBACK_AGENT_ID_HEADER: &str = "x-tyde-agent-id";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";

#[derive(Clone, Debug)]
pub(crate) struct ReviewMcpHandle {
    pub(crate) url: String,
}

#[derive(Clone)]
struct TydeReviewMcpServer {
    host: HostHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeReviewMcpServer {
    fn new(host: HostHandle) -> Self {
        Self {
            host,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProposeReviewCommentToolInput {
    review_id: ReviewId,
    location: ReviewLocation,
    body: String,
    severity: ReviewSeverity,
    rationale: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ProposeReviewCommentToolResult {
    Success {
        suggestion_id: ReviewSuggestionId,
    },
    ValidationError {
        code: ReviewErrorCode,
        message: String,
    },
}

#[derive(Serialize)]
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
impl TydeReviewMcpServer {
    #[tool(
        description = "Propose a pending inline review comment for the supplied frozen review_id and typed diff location."
    )]
    async fn propose_review_comment(
        &self,
        Parameters(input): Parameters<ProposeReviewCommentToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let reviewer_agent_id = match request_agent_id_from_parts(&parts) {
            Ok(Some(agent_id)) => agent_id,
            Ok(None) => return Ok(err_text("propose_review_comment requires calling agent_id")),
            Err(err) => return Ok(err_text(err)),
        };
        let review_id = input.review_id;
        let args = ProposeReviewCommentArgs {
            location: input.location,
            body: input.body,
            severity: input.severity,
            rationale: input.rationale,
        };
        let Some(suggestion) =
            ReviewerToolBridge::suggestion_from_tool_args(&reviewer_agent_id, args)
        else {
            return ok_json(ProposeReviewCommentToolResult::ValidationError {
                code: ReviewErrorCode::InvalidStatus,
                message: "suggestion body must not be empty".to_owned(),
            });
        };
        match self
            .host
            .propose_review_comment(review_id, suggestion)
            .await
        {
            Ok(Ok(suggestion_id)) => {
                ok_json(ProposeReviewCommentToolResult::Success { suggestion_id })
            }
            Ok(Err(error)) => ok_json(ProposeReviewCommentToolResult::ValidationError {
                code: error.code,
                message: error.message,
            }),
            Err(error) => ok_json(ProposeReviewCommentToolResult::ValidationError {
                code: ReviewErrorCode::Internal,
                message: error,
            }),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeReviewMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Review feedback tools for Tyde AI reviewers. Use propose_review_comment to create pending review suggestions; this server has no agent orchestration tools."
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
) -> Result<ReviewMcpHandle, String> {
    let bind_addr = bind_addr.unwrap_or_else(|| {
        DEFAULT_BIND_ADDR
            .parse()
            .expect("default loopback review MCP bind addr must parse")
    });
    if !bind_addr.ip().is_loopback() {
        return Err(format!(
            "review MCP server must bind to loopback only, got {bind_addr}"
        ));
    }

    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind review MCP HTTP server on {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set review MCP listener nonblocking: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read review MCP listener addr: {err}"))?;

    std::thread::Builder::new()
        .name("tyde-review-mcp".to_string())
        .spawn(move || {
            let runtime = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build review MCP runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failed to create tokio review MCP listener");
                let mcp_service: StreamableHttpService<TydeReviewMcpServer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(TydeReviewMcpServer::new(host_handle.clone())),
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
                    tracing::warn!("review MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn review MCP server thread: {err}"))?;

    Ok(ReviewMcpHandle {
        url: format!("http://{local_addr}/mcp"),
    })
}

fn request_agent_id_from_parts(
    parts: &axum::http::request::Parts,
) -> Result<Option<AgentId>, String> {
    if let Some(agent_id) = parts
        .headers
        .get(REVIEW_FEEDBACK_AGENT_ID_HEADER)
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
