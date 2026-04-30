use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::{Json, Router, response::IntoResponse, routing::get};
use protocol::{
    AgentControlStatus, AgentId, AgentInput, AgentOrigin, BackendKind, Envelope, ProjectId,
    SendMessagePayload, SpawnAgentParams, SpawnAgentPayload, SpawnCostHint,
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
use serde_json::json;
use tokio::time::timeout;
use uuid::Uuid;

use crate::host::HostHandle;

pub const AGENT_CONTROL_AGENT_ID_HEADER: &str = "x-tyde-agent-id";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";
const DEFAULT_AWAIT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_READ_LIMIT: usize = 100;
const MAX_READ_LIMIT: usize = 1_000;

#[derive(Clone, Debug)]
pub struct AgentControlMcpHandle {
    pub url: String,
}

#[derive(Clone)]
struct TydeAgentControlMcpServer {
    host: HostHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeAgentControlMcpServer {
    fn new(host: HostHandle) -> Self {
        Self {
            host,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BackendKindInput {
    Tycode,
    Kiro,
    Claude,
    Codex,
    Gemini,
}

impl From<BackendKindInput> for BackendKind {
    fn from(value: BackendKindInput) -> Self {
        match value {
            BackendKindInput::Tycode => Self::Tycode,
            BackendKindInput::Kiro => Self::Kiro,
            BackendKindInput::Claude => Self::Claude,
            BackendKindInput::Codex => Self::Codex,
            BackendKindInput::Gemini => Self::Gemini,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CostHintInput {
    Low,
    Med,
    High,
}

impl From<CostHintInput> for SpawnCostHint {
    fn from(value: CostHintInput) -> Self {
        match value {
            CostHintInput::Low => Self::Low,
            CostHintInput::Med => Self::Medium,
            CostHintInput::High => Self::High,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnAgentToolInput {
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<BackendKindInput>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AwaitAgentsToolInput {
    agent_ids: Vec<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadAgentToolInput {
    agent_id: String,
    after_seq: Option<u64>,
    limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendAgentMessageToolInput {
    agent_id: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Serialize)]
struct SpawnAgentResult {
    agent_id: String,
    name: String,
    status: AgentControlStatus,
}

#[derive(Debug, Serialize)]
struct AwaitAgentStatus {
    agent_id: String,
    status: AgentControlStatus,
}

#[derive(Debug, Serialize)]
struct AwaitAgentsResult {
    ready: Vec<AwaitAgentStatus>,
    still_thinking: Vec<AwaitAgentStatus>,
}

#[derive(Debug, Serialize)]
struct ReadAgentResult {
    agent_id: String,
    events: Vec<Envelope>,
    next_after_seq: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AgentOverview {
    agent_id: String,
    name: String,
    backend_kind: BackendKind,
    origin: AgentOrigin,
    status: AgentControlStatus,
    workspace_roots: Vec<String>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    created_at_ms: u64,
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

fn request_agent_id_from_parts(
    parts: &axum::http::request::Parts,
) -> Result<Option<AgentId>, String> {
    if let Some(agent_id) = parts
        .headers
        .get(AGENT_CONTROL_AGENT_ID_HEADER)
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

#[tool_router]
impl TydeAgentControlMcpServer {
    #[tool(description = "Spawn a Tyde agent and return immediately with its agent_id.")]
    async fn tyde_spawn_agent(
        &self,
        Parameters(input): Parameters<SpawnAgentToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id = match request_agent_id_from_parts(&parts) {
            Ok(agent_id) => agent_id,
            Err(err) => return Ok(err_text(err)),
        };
        match do_spawn_agent(&self.host, input.into(), request_agent_id).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Wait until any supplied Tyde agent becomes idle or failed. Returns statuses only; use tyde_read_agent to read output."
    )]
    async fn tyde_await_agents(
        &self,
        Parameters(input): Parameters<AwaitAgentsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let agent_ids = match parse_agent_ids(input.agent_ids) {
            Ok(ids) => ids,
            Err(err) => return Ok(err_text(err)),
        };
        match do_await_agents(&self.host, agent_ids, input.timeout_ms).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Read output events from a Tyde agent. This is the only agent-control tool that returns agent output."
    )]
    async fn tyde_read_agent(
        &self,
        Parameters(input): Parameters<ReadAgentToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let agent_id = match parse_agent_id(&input.agent_id) {
            Ok(id) => id,
            Err(err) => return Ok(err_text(err)),
        };
        match do_read_agent(&self.host, &agent_id, input.after_seq, input.limit).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Send a follow-up message to an existing Tyde agent.")]
    async fn tyde_send_agent_message(
        &self,
        Parameters(input): Parameters<SendAgentMessageToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let agent_id = match parse_agent_id(&input.agent_id) {
            Ok(id) => id,
            Err(err) => return Ok(err_text(err)),
        };
        if input.message.trim().is_empty() {
            return Ok(err_text("message must not be empty"));
        }
        match do_send_message(&self.host, &agent_id, input.message).await {
            Ok(()) => ok_json(json!({ "ok": true })),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "List all agents currently known to this Tyde host.")]
    async fn tyde_list_agents(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match do_list_agents(&self.host).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeAgentControlMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for orchestrating Tyde2 coding agents. Spawn agents with tyde_spawn_agent, wait for them with tyde_await_agents, send follow-ups with tyde_send_agent_message, and read output only with tyde_read_agent."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

pub fn start_server(
    bind_addr: Option<SocketAddr>,
    host_handle: HostHandle,
) -> Result<AgentControlMcpHandle, String> {
    let bind_addr = bind_addr.unwrap_or_else(|| {
        DEFAULT_BIND_ADDR
            .parse()
            .expect("default loopback agent-control MCP bind addr must parse")
    });
    if !bind_addr.ip().is_loopback() {
        return Err(format!(
            "agent-control MCP server must bind to loopback only, got {bind_addr}"
        ));
    }

    let listener = std::net::TcpListener::bind(bind_addr).map_err(|err| {
        format!("failed to bind agent-control MCP HTTP server on {bind_addr}: {err}")
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set agent-control MCP listener nonblocking: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read agent-control MCP listener addr: {err}"))?;

    std::thread::Builder::new()
        .name("tyde-agent-control-mcp".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build agent-control MCP runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failed to create tokio agent-control MCP listener");
                let mcp_service: StreamableHttpService<
                    TydeAgentControlMcpServer,
                    LocalSessionManager,
                > = StreamableHttpService::new(
                    move || Ok(TydeAgentControlMcpServer::new(host_handle.clone())),
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
                    tracing::warn!("agent-control MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn agent-control MCP server thread: {err}"))?;

    Ok(AgentControlMcpHandle {
        url: format!("http://{local_addr}/mcp"),
    })
}

async fn do_spawn_agent(
    host: &HostHandle,
    input: SpawnRequestInput,
    request_agent_id: Option<AgentId>,
) -> Result<SpawnAgentResult, String> {
    if input.workspace_roots.is_empty() {
        return Err("workspace_roots must contain at least one root".to_string());
    }
    if input.workspace_roots.iter().any(|r| r.trim().is_empty()) {
        return Err("workspace_roots must not contain empty values".to_string());
    }
    if input.prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }

    let host_settings = host.read_settings().await?;
    let backend_kind = input
        .backend_kind
        .map(BackendKind::from)
        .or(host_settings.default_backend)
        .ok_or_else(|| {
            "backend_kind is required because the host has no default_backend".to_string()
        })?;

    let project_id = input
        .project_id
        .as_deref()
        .map(parse_project_id)
        .transpose()?;
    let explicit_parent = input
        .parent_agent_id
        .as_deref()
        .map(parse_agent_id)
        .transpose()?;
    let parent_agent_id = explicit_parent.or(request_agent_id);
    let requested_name = input.name.filter(|value| !value.trim().is_empty());

    let payload = SpawnAgentPayload {
        name: requested_name.clone(),
        custom_agent_id: None,
        parent_agent_id,
        project_id,
        params: SpawnAgentParams::New {
            workspace_roots: input.workspace_roots,
            prompt: input.prompt,
            images: None,
            backend_kind,
            cost_hint: input.cost_hint.map(SpawnCostHint::from),
            session_settings: None,
        },
    };

    let agent_id = host.spawn_agent_and_return_id(payload).await;
    let status = host.agent_status_snapshot(&agent_id).await;
    let agent_status = status
        .as_ref()
        .map(|s| s.status())
        .unwrap_or(AgentControlStatus::Thinking);
    let name = host
        .list_agents()
        .await
        .into_iter()
        .find(|start| start.agent_id == agent_id)
        .map(|start| start.name)
        .ok_or_else(|| format!("spawned agent {} missing from host registry", agent_id.0))?;

    Ok(SpawnAgentResult {
        agent_id: agent_id.0,
        name,
        status: agent_status,
    })
}

async fn do_send_message(
    host: &HostHandle,
    agent_id: &AgentId,
    message: String,
) -> Result<(), String> {
    let handle = host
        .agent_handle(agent_id)
        .await
        .ok_or_else(|| format!("unknown agent_id {}", agent_id.0))?;

    // Mark the agent active again before forwarding the follow-up turn.
    if let Some(status_handle) = host.agent_status_handle(agent_id).await {
        status_handle
            .update(|s| {
                s.turn_completed = false;
                s.activity_counter = s.activity_counter.saturating_add(1);
            })
            .await;
    }

    let sent = handle
        .send_input(AgentInput::SendMessage(SendMessagePayload {
            message,
            images: None,
        }))
        .await;
    if !sent {
        return Err("agent backend is closed".to_string());
    }
    Ok(())
}

async fn do_list_agents(host: &HostHandle) -> Result<Vec<AgentOverview>, String> {
    let agents = host.list_agents().await;
    let mut overviews = Vec::with_capacity(agents.len());
    for start in agents {
        let status = host
            .agent_status_snapshot(&start.agent_id)
            .await
            .ok_or_else(|| format!("missing status for agent_id {}", start.agent_id.0))?;
        overviews.push(AgentOverview {
            agent_id: start.agent_id.0,
            name: start.name,
            backend_kind: start.backend_kind,
            origin: start.origin,
            status: status.status(),
            workspace_roots: start.workspace_roots,
            parent_agent_id: start.parent_agent_id.map(|id| id.0),
            project_id: start.project_id.map(|id| id.0),
            created_at_ms: start.created_at_ms,
        });
    }
    overviews.sort_by_key(|o| o.created_at_ms);
    Ok(overviews)
}

async fn do_await_agents(
    host: &HostHandle,
    agent_ids: Vec<AgentId>,
    timeout_ms: Option<u64>,
) -> Result<AwaitAgentsResult, String> {
    if agent_ids.is_empty() {
        return Err("agent_ids must contain at least one agent_id".to_string());
    }

    for agent_id in &agent_ids {
        if host.agent_status_snapshot(agent_id).await.is_none() {
            return Err(format!("unknown agent_id {}", agent_id.0));
        }
    }

    let timeout_at =
        Instant::now() + Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_AWAIT_TIMEOUT_MS));
    let mut status_rx = host.subscribe_agent_status_changes().await;

    loop {
        let result = await_result_from_snapshot(host, &agent_ids).await?;
        if !result.ready.is_empty() || result.still_thinking.is_empty() {
            return Ok(result);
        }

        let remaining = timeout_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(result);
        }

        match timeout(remaining, status_rx.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err("agent status notification channel closed".to_string()),
            Err(_) => return await_result_from_snapshot(host, &agent_ids).await,
        }
    }
}

async fn await_result_from_snapshot(
    host: &HostHandle,
    agent_ids: &[AgentId],
) -> Result<AwaitAgentsResult, String> {
    let mut ready = Vec::new();
    let mut still_thinking = Vec::new();

    for agent_id in agent_ids {
        let status = host
            .agent_status_snapshot(agent_id)
            .await
            .ok_or_else(|| format!("unknown agent_id {}", agent_id.0))?;
        let entry = AwaitAgentStatus {
            agent_id: agent_id.0.clone(),
            status: status.status(),
        };
        if status.is_active() {
            still_thinking.push(entry);
        } else {
            ready.push(entry);
        }
    }

    Ok(AwaitAgentsResult {
        ready,
        still_thinking,
    })
}

async fn do_read_agent(
    host: &HostHandle,
    agent_id: &AgentId,
    after_seq: Option<u64>,
    limit: Option<u32>,
) -> Result<ReadAgentResult, String> {
    let limit = limit
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_READ_LIMIT);
    if limit == 0 {
        return Err("limit must be greater than zero".to_string());
    }
    if limit > MAX_READ_LIMIT {
        return Err(format!("limit must be <= {MAX_READ_LIMIT}"));
    }

    let handle = host
        .agent_handle(agent_id)
        .await
        .ok_or_else(|| format!("unknown agent_id {}", agent_id.0))?;
    let events = handle
        .read_output(after_seq, limit)
        .await
        .ok_or_else(|| format!("agent {} is not available", agent_id.0))?;
    let next_after_seq = events.last().map(|event| event.seq).or(after_seq);

    Ok(ReadAgentResult {
        agent_id: agent_id.0.clone(),
        events,
        next_after_seq,
    })
}

#[derive(Debug)]
struct SpawnRequestInput {
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<BackendKindInput>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
}

impl From<SpawnAgentToolInput> for SpawnRequestInput {
    fn from(v: SpawnAgentToolInput) -> Self {
        Self {
            workspace_roots: v.workspace_roots,
            prompt: v.prompt,
            backend_kind: v.backend_kind,
            parent_agent_id: v.parent_agent_id,
            project_id: v.project_id,
            name: v.name,
            cost_hint: v.cost_hint,
        }
    }
}

fn parse_agent_id(input: &str) -> Result<AgentId, String> {
    Uuid::parse_str(input).map_err(|err| format!("invalid agent_id '{input}': {err}"))?;
    Ok(AgentId(input.to_string()))
}

fn parse_agent_ids(inputs: Vec<String>) -> Result<Vec<AgentId>, String> {
    let mut agent_ids = Vec::with_capacity(inputs.len());
    for input in inputs {
        agent_ids.push(parse_agent_id(&input)?);
    }
    Ok(agent_ids)
}

fn parse_project_id(input: &str) -> Result<ProjectId, String> {
    Uuid::parse_str(input).map_err(|err| format!("invalid project_id '{input}': {err}"))?;
    Ok(ProjectId(input.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_bind_addr() {
        // The loopback check happens before the host handle is used, so we need
        // a real HostHandle but it won't be accessed.
        let dir = std::env::temp_dir().join("tyde-ac-mcp-test");
        let _ = std::fs::create_dir_all(&dir);
        let sp = dir.join("sessions.json");
        let pp = dir.join("projects.json");
        let stp = dir.join("settings.json");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let host = rt
            .block_on(async { crate::host::spawn_host_with_mock_backend(sp, pp, stp) })
            .expect("mock host");

        let err = start_server(
            Some(
                "0.0.0.0:0"
                    .parse()
                    .expect("wildcard socket addr should parse"),
            ),
            host,
        )
        .expect_err("non-loopback bind addr should be rejected");
        assert!(err.contains("loopback only"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_request_target_reads_percent_encoded_agent_id() {
        let agent_id = "550e8400-e29b-41d4-a716-446655440000";
        let target = format!("/mcp?agent_id={agent_id}&ignored=value");
        let (path, parsed_agent_id) =
            split_request_target(&target).expect("request target should parse");
        assert_eq!(path, "/mcp");
        assert_eq!(parsed_agent_id, Some(AgentId(agent_id.to_string())));
    }

    #[test]
    fn split_request_target_rejects_invalid_agent_id() {
        let err = split_request_target("/mcp?agent_id=not-a-uuid")
            .expect_err("invalid agent_id should fail");
        assert!(err.contains("invalid agent_id"));
    }
}
