use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{Json, Router, response::IntoResponse, routing::get};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use protocol::{
    AGENT_CONTROL_DEFAULT_READ_LIMIT, AGENT_CONTROL_DEFAULT_READ_MAX_BYTES,
    AGENT_CONTROL_MAX_READ_LIMIT, AGENT_CONTROL_MAX_READ_MAX_BYTES, AgentControlReadDebugResult,
    AgentControlReadResult, AgentControlStatus, AgentId, AgentInput, AgentOrigin,
    BackendAccessMode, BackendKind, CustomAgentId, GitBranchName, ImageData, LaunchProfileCatalog,
    LaunchProfileId, ProjectId, ProjectSource, SendMessagePayload, SessionSchemaEntry,
    SessionSettingsValues, SpawnAgentParams, SpawnAgentPayload, SpawnCostHint, Team, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, WorkbenchCreatePayload, WorkflowSaveRequest,
    WorkflowSaveResponse, WorkflowTargetsResponse, cap_agent_control_events,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        tool::{Extension, ToolCallContext},
        wrapper::Parameters,
    },
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ProgressNotificationParam, ProgressToken, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::{Peer, RequestContext},
    tool, tool_router,
    transport::{
        StreamableHttpServerConfig,
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::host::{BaseRevision, CreatedWorkbench, HostHandle};
use crate::team_registry::team_preset_catalog;

pub const AGENT_CONTROL_AGENT_ID_HEADER: &str = "x-tyde-agent-id";
pub const AGENT_CONTROL_MCP_SERVER_NAME: &str = "tyde-agent-control";
pub const AGENT_CONTROL_AWAIT_MCP_SERVER_NAME: &str = "tyde-agent-await";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";
const AWAIT_TOOL_PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct AgentControlMcpHandle {
    pub url: String,
    pub await_url: String,
    credentials: AgentControlCredentialAuthority,
}

#[derive(Clone)]
pub struct AgentControlMcpCaller {
    pub url: String,
    pub await_url: String,
    pub authorization: String,
}

impl std::fmt::Debug for AgentControlMcpCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentControlMcpCaller")
            .field("url", &self.url)
            .field("await_url", &self.await_url)
            .field("authorization", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
struct AgentControlCredentialAuthority {
    secret: Arc<[u8; 32]>,
}

impl std::fmt::Debug for AgentControlCredentialAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentControlCredentialAuthority")
            .finish_non_exhaustive()
    }
}

impl AgentControlCredentialAuthority {
    fn new() -> Self {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut secret = [0_u8; 32];
        secret[..16].copy_from_slice(first.as_bytes());
        secret[16..].copy_from_slice(second.as_bytes());
        Self {
            secret: Arc::new(secret),
        }
    }

    fn issue(&self, agent_id: &AgentId) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_ref())
            .expect("HMAC-SHA256 accepts a 32-byte key");
        mac.update(agent_id.0.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("v1.{}.{signature}", agent_id.0)
    }

    fn verify(&self, token: &str) -> Result<AgentId, String> {
        let mut parts = token.split('.');
        let version = parts.next();
        let agent_id = parts.next();
        let signature = parts.next();
        if version != Some("v1") || parts.next().is_some() {
            return Err("invalid agent-control bearer credential".to_owned());
        }
        let agent_id =
            agent_id.ok_or_else(|| "invalid agent-control bearer credential".to_owned())?;
        let signature = signature
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
            .ok_or_else(|| "invalid agent-control bearer credential".to_owned())?;
        let agent_id = parse_agent_id(agent_id)
            .map_err(|_| "invalid agent-control bearer credential".to_owned())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_ref())
            .expect("HMAC-SHA256 accepts a 32-byte key");
        mac.update(agent_id.0.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| "invalid agent-control bearer credential".to_owned())?;
        Ok(agent_id)
    }
}

impl AgentControlMcpHandle {
    pub(crate) fn disabled() -> Self {
        Self {
            url: String::new(),
            await_url: String::new(),
            credentials: AgentControlCredentialAuthority::new(),
        }
    }

    pub(crate) fn caller(&self, agent_id: &AgentId) -> AgentControlMcpCaller {
        AgentControlMcpCaller {
            url: self.url.clone(),
            await_url: self.await_url.clone(),
            authorization: format!("Bearer {}", self.credentials.issue(agent_id)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentControlMcpSurface {
    Control,
    Await,
}

#[derive(Clone)]
struct TydeAgentControlMcpServer {
    host: HostHandle,
    credentials: AgentControlCredentialAuthority,
    surface: AgentControlMcpSurface,
    tool_router: ToolRouter<Self>,
}

impl TydeAgentControlMcpServer {
    fn new(
        host: HostHandle,
        credentials: AgentControlCredentialAuthority,
        surface: AgentControlMcpSurface,
    ) -> Self {
        Self {
            host,
            credentials,
            surface,
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
    Antigravity,
    Hermes,
}

impl From<BackendKindInput> for BackendKind {
    fn from(value: BackendKindInput) -> Self {
        match value {
            BackendKindInput::Tycode => Self::Tycode,
            BackendKindInput::Kiro => Self::Kiro,
            BackendKindInput::Claude => Self::Claude,
            BackendKindInput::Codex => Self::Codex,
            BackendKindInput::Antigravity => Self::Antigravity,
            BackendKindInput::Hermes => Self::Hermes,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BackendAccessModeInput {
    Unrestricted,
    ReadOnly,
}

impl From<BackendAccessModeInput> for BackendAccessMode {
    fn from(value: BackendAccessModeInput) -> Self {
        match value {
            BackendAccessModeInput::Unrestricted => Self::Unrestricted,
            BackendAccessModeInput::ReadOnly => Self::ReadOnly,
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
    #[serde(default)]
    workspace_roots: Vec<String>,
    prompt: String,
    launch_profile_id: Option<String>,
    backend_kind: Option<BackendKindInput>,
    session_settings: Option<SessionSettingsValues>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    /// Task complexity. `low`: trivial task that needs no real reasoning —
    /// runs on a cheaper/faster configuration. `high`: extremely complex
    /// task — runs on the most capable configuration. Omit for normal tasks.
    cost_hint: Option<CostHintInput>,
    access_mode: Option<BackendAccessModeInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AwaitAgentsToolInput {
    /// One or more non-empty direct child agent IDs. Pass every child whose
    /// transition to idle or failed should wake this wait.
    #[schemars(length(min = 1), inner(length(min = 1)))]
    agent_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadAgentToolInput {
    agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadAgentDebugToolInput {
    agent_id: String,
    after_seq: Option<u64>,
    limit: Option<u32>,
    max_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendAgentMessageToolInput {
    #[schemars(length(min = 1))]
    agent_id: String,
    #[schemars(length(min = 1))]
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TeamMessageMemberToolInput {
    member_id: String,
    message: String,
    images: Option<Vec<TeamMessageImageInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TeamMessageImageInput {
    media_type: String,
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateWorkbenchToolInput {
    parent_project_id: String,
    branch: String,
    name: Option<String>,
    base_ref: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateWorkbenchResult {
    project_id: String,
    name: String,
    branch: String,
    parent_project_id: String,
    roots: Vec<CreatedWorkbenchRootResult>,
}

#[derive(Debug, Serialize)]
struct CreatedWorkbenchRootResult {
    parent_root: String,
    worktree_root: String,
    base_commit: String,
    parent_root_dirty: bool,
}

#[derive(Debug, Serialize)]
struct ListWorkbenchesResult {
    caller_project_id: String,
    projects: Vec<ProjectOverview>,
}

#[derive(Debug, Serialize)]
struct ProjectOverview {
    project_id: String,
    name: String,
    kind: ProjectKindOutput,
    parent_project_id: Option<String>,
    branch: Option<String>,
    workspace_roots: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProjectKindOutput {
    Standalone,
    Workbench,
}

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
struct ListLaunchOptionsResult {
    catalog: LaunchProfileCatalog,
    default_backend: Option<BackendKind>,
    session_schemas: Vec<SessionSchemaEntry>,
}

#[derive(Clone)]
struct AwaitProgressReporter {
    peer: Peer<RoleServer>,
    progress_token: ProgressToken,
    interval: Duration,
}

impl AwaitProgressReporter {
    fn from_context(context: &RequestContext<RoleServer>) -> Option<Self> {
        context
            .meta
            .get_progress_token()
            .map(|progress_token| Self {
                peer: context.peer.clone(),
                progress_token,
                interval: AWAIT_TOOL_PROGRESS_INTERVAL,
            })
    }

    async fn notify(&self, progress: f64, still_thinking_count: usize) {
        let message = format!("Waiting for {still_thinking_count} Tyde agent(s)");
        let _ = self
            .peer
            .notify_progress(ProgressNotificationParam {
                progress_token: self.progress_token.clone(),
                progress,
                total: None,
                message: Some(message),
            })
            .await;
    }
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

#[derive(Debug, Serialize)]
struct TeamDescribeResult {
    team: Team,
    members: Vec<TeamDescribeMember>,
}

#[derive(Debug, Serialize)]
struct TeamDescribeMember {
    member: TeamMember,
    profile: Option<TeamProfileSummary>,
    custom_agent: Option<TeamCustomAgentSummary>,
    binding: TeamMemberBindingPayload,
}

#[derive(Debug, Serialize)]
struct TeamProfileSummary {
    role_preset: Option<String>,
    personality_preset: Option<String>,
    traits: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TeamCustomAgentSummary {
    id: CustomAgentId,
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct TeamMessageMemberResult {
    member_id: String,
    agent_id: String,
    queued: bool,
}

#[derive(Debug, Serialize)]
struct TeamToolError {
    code: TeamToolErrorCode,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum TeamToolErrorCode {
    Authorization,
    Conflict,
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

fn err_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::json(value)?]))
}

fn claimed_agent_id_from_parts(
    parts: &axum::http::request::Parts,
) -> Result<Option<AgentId>, String> {
    let header_agent_id = parts
        .headers
        .get(AGENT_CONTROL_AGENT_ID_HEADER)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| "x-tyde-agent-id header must be UTF-8".to_owned())
                .and_then(|value| parse_agent_id(value.trim()))
        })
        .transpose()?;

    let target = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let (_, query_agent_id) = split_request_target(target)?;
    match (header_agent_id, query_agent_id) {
        (Some(header), Some(query)) if header != query => {
            Err("x-tyde-agent-id header does not match agent_id query parameter".to_owned())
        }
        (Some(agent_id), _) | (_, Some(agent_id)) => Ok(Some(agent_id)),
        (None, None) => Ok(None),
    }
}

fn authenticated_caller_from_parts(
    credentials: &AgentControlCredentialAuthority,
    parts: &axum::http::request::Parts,
) -> Result<Option<AgentId>, String> {
    let claimed_agent_id = claimed_agent_id_from_parts(parts)?;
    let authorization = parts.headers.get(axum::http::header::AUTHORIZATION);
    let Some(authorization) = authorization else {
        if claimed_agent_id.is_some() {
            return Err("agent identity requires an agent-control bearer credential".to_owned());
        }
        return Ok(None);
    };
    let authorization = authorization
        .to_str()
        .map_err(|_| "authorization header must be UTF-8".to_owned())?;
    let token = authorization
        .strip_prefix("Bearer ")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "authorization header must contain a Bearer credential".to_owned())?;
    let authenticated_agent_id = credentials.verify(token.trim())?;
    if claimed_agent_id
        .as_ref()
        .is_some_and(|claimed| claimed != &authenticated_agent_id)
    {
        return Err("agent identity header/query does not match bearer credential".to_owned());
    }
    Ok(Some(authenticated_agent_id))
}

async fn require_authenticated_caller(
    server: &TydeAgentControlMcpServer,
    parts: &axum::http::request::Parts,
    tool_name: &'static str,
) -> Result<AgentId, String> {
    let caller = authenticated_caller_from_parts(&server.credentials, parts)?
        .ok_or_else(|| format!("{tool_name} requires an agent-control bearer credential"))?;
    if server.host.agent_handle(&caller).await.is_none() {
        return Err("authenticated agent-control caller is not active".to_owned());
    }
    Ok(caller)
}

async fn authorize_direct_children(
    host: &HostHandle,
    caller: &AgentId,
    targets: &[AgentId],
) -> Result<(), String> {
    let agents = host.list_agents().await;
    for target in targets {
        let authorized = agents.iter().any(|agent| {
            agent.agent_id == *target && agent.parent_agent_id.as_ref() == Some(caller)
        });
        if !authorized {
            return Err(format!(
                "authorization: agent_id {} is not a direct child of caller {}",
                target.0, caller.0
            ));
        }
    }
    Ok(())
}

#[tool_router]
impl TydeAgentControlMcpServer {
    #[tool(
        description = "Spawn a direct child of the authenticated caller and return immediately with its agent_id."
    )]
    async fn tyde_spawn_agent(
        &self,
        Parameters(input): Parameters<SpawnAgentToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_spawn_agent").await {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        match do_spawn_agent(&self.host, input.into(), Some(caller)).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Create a git workbench under the authenticated caller's project. Defaults to each parent root's HEAD; base_ref is resolved in every root before mutation. Uncommitted and untracked parent changes are disclosed but never copied. On an unexpected branch/path conflict, stop and report it rather than retrying with another name."
    )]
    async fn tyde_create_workbench(
        &self,
        Parameters(input): Parameters<CreateWorkbenchToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_create_workbench").await
        {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        if let Err(error) = reject_mutating_tool_for_read_only_caller(
            &self.host,
            Some(&caller),
            "tyde_create_workbench",
        )
        .await
        {
            return Ok(err_text(error));
        }
        match do_create_workbench(&self.host, &caller, input).await {
            Ok(result) => ok_json(result),
            Err(error) => Ok(err_text(error)),
        }
    }

    #[tool(
        description = "List the authenticated caller's canonical project and its git workbenches for safe creation recovery and project_id-based spawning."
    )]
    async fn tyde_list_workbenches(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_list_workbenches").await
        {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        match do_list_workbenches(&self.host, &caller).await {
            Ok(result) => ok_json(result),
            Err(error) => Ok(err_text(error)),
        }
    }

    #[tool(description = "List server-owned Launch Profiles and backend launch metadata.")]
    async fn tyde_list_launch_options(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match do_list_launch_options(&self.host).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Wait without a Tyde tool timer until any supplied direct child becomes idle or failed. agent_ids is required and must contain at least one non-empty direct child ID. Requires the calling agent's bearer credential and returns statuses only."
    )]
    async fn tyde_await_agents(
        &self,
        Parameters(input): Parameters<AwaitAgentsToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_await_agents").await {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        let agent_ids = match parse_agent_ids(input.agent_ids) {
            Ok(ids) => ids,
            Err(err) => return Ok(err_text(err)),
        };
        if let Err(error) = authorize_direct_children(&self.host, &caller, &agent_ids).await {
            return Ok(err_text(error));
        }
        match do_await_agents(&self.host, agent_ids, context).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Read only a direct child's server-owned latest assistant-visible message, error, or empty record. Never scans backward."
    )]
    async fn tyde_read_agent(
        &self,
        Parameters(input): Parameters<ReadAgentToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_read_agent").await {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        let agent_id = match parse_agent_id(&input.agent_id) {
            Ok(id) => id,
            Err(err) => return Ok(err_text(err)),
        };
        if let Err(error) =
            authorize_direct_children(&self.host, &caller, std::slice::from_ref(&agent_id)).await
        {
            return Ok(err_text(error));
        }
        match do_read_agent(&self.host, &agent_id).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Debug-only detailed incremental output events for a direct child. Results are capped by limit and max_bytes."
    )]
    async fn tyde_read_agent_debug(
        &self,
        Parameters(input): Parameters<ReadAgentDebugToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match require_authenticated_caller(self, &parts, "tyde_read_agent_debug").await
        {
            Ok(caller) => caller,
            Err(error) => return Ok(err_text(error)),
        };
        let agent_id = match parse_agent_id(&input.agent_id) {
            Ok(id) => id,
            Err(err) => return Ok(err_text(err)),
        };
        if let Err(error) =
            authorize_direct_children(&self.host, &caller, std::slice::from_ref(&agent_id)).await
        {
            return Ok(err_text(error));
        }
        match do_read_agent_debug(
            &self.host,
            &agent_id,
            input.after_seq,
            input.limit,
            input.max_bytes,
        )
        .await
        {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Send a follow-up message to a direct child of the authenticated caller.")]
    async fn tyde_send_agent_message(
        &self,
        Parameters(input): Parameters<SendAgentMessageToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id =
            match require_authenticated_caller(self, &parts, "tyde_send_agent_message").await {
                Ok(agent_id) => agent_id,
                Err(err) => return Ok(err_text(err)),
            };
        let agent_id = match parse_agent_id(&input.agent_id) {
            Ok(id) => id,
            Err(err) => return Ok(err_text(err)),
        };
        if input.message.trim().is_empty() {
            return Ok(err_text("message must not be empty"));
        }
        if let Err(error) = authorize_direct_children(
            &self.host,
            &request_agent_id,
            std::slice::from_ref(&agent_id),
        )
        .await
        {
            return Ok(err_text(error));
        }
        match do_send_message(&self.host, &agent_id, input.message, Some(request_agent_id)).await {
            Ok(()) => ok_json(json!({ "ok": true })),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Describe the calling team member's team, roster, optional custom-agent summaries, and live bindings."
    )]
    async fn tyde_team_describe(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id =
            match require_authenticated_caller(self, &parts, "tyde_team_describe").await {
                Ok(agent_id) => agent_id,
                Err(err) => return Ok(err_text(err)),
            };
        match do_team_describe(&self.host, request_agent_id).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Manager-only: send a message to an active report. Returns the report member_id and live agent_id."
    )]
    async fn tyde_team_message_member(
        &self,
        Parameters(input): Parameters<TeamMessageMemberToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id =
            match require_authenticated_caller(self, &parts, "tyde_team_message_member").await {
                Ok(agent_id) => agent_id,
                Err(err) => return Ok(err_text(err)),
            };
        if let Err(err) = reject_mutating_tool_for_read_only_caller(
            &self.host,
            Some(&request_agent_id),
            "tyde_team_message_member",
        )
        .await
        {
            return Ok(err_text(err));
        }
        match do_team_message_member(&self.host, request_agent_id, input).await {
            Ok(result) => ok_json(result),
            Err(err) if err.starts_with("authorization:") => err_json(TeamToolError {
                code: TeamToolErrorCode::Authorization,
                message: err,
            }),
            Err(err) if err.starts_with("conflict:") => err_json(TeamToolError {
                code: TeamToolErrorCode::Conflict,
                message: err,
            }),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Return valid Tyde workflow target directories for this caller context.")]
    async fn tyde_workflow_targets(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id = match authenticated_caller_from_parts(&self.credentials, &parts) {
            Ok(agent_id) => agent_id,
            Err(err) => return Ok(err_text(err)),
        };
        match do_workflow_targets(&self.host, request_agent_id.as_ref()).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Validate and save one Tyde workflow Markdown file, then reload the catalog."
    )]
    async fn tyde_workflow_save(
        &self,
        Parameters(input): Parameters<WorkflowSaveRequest>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id = match authenticated_caller_from_parts(&self.credentials, &parts) {
            Ok(agent_id) => agent_id,
            Err(err) => return Ok(err_text(err)),
        };
        if let Err(err) = reject_mutating_tool_for_read_only_caller(
            &self.host,
            request_agent_id.as_ref(),
            "tyde_workflow_save",
        )
        .await
        {
            return Ok(err_text(err));
        }
        match do_workflow_save(&self.host, input).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "List only agents directly created by the calling Tyde agent.")]
    async fn tyde_list_agents(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let request_agent_id =
            match require_authenticated_caller(self, &parts, "tyde_list_agents").await {
                Ok(agent_id) => agent_id,
                Err(err) => return Ok(err_text(err)),
            };
        match do_list_agents(&self.host, &request_agent_id).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }
}

impl ServerHandler for TydeAgentControlMcpServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = match self.surface {
            AgentControlMcpSurface::Control => {
                "Tools for orchestrating direct child Tyde agents. Spawn agents, send follow-ups, read the latest visible output, inspect incremental debug events, and list direct children. Long-running waits are exposed by the separate tyde-agent-await MCP server."
            }
            AgentControlMcpSurface::Await => {
                "The dedicated long-running tyde_await_agents tool for direct child Tyde agents."
            }
        };
        ServerInfo {
            instructions: Some(instructions.into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    // Hand-written (instead of #[tool_handler]) so the tool list can be
    // filtered against host settings: when task complexity tiers are
    // disabled, the cost_hint field is hidden from the spawn tool schema so
    // agents never pick a tier. The host spawn path independently ignores
    // hints while tiers are disabled, so a stale schema can't re-enable them.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        tools.retain(|tool| match self.surface {
            AgentControlMcpSurface::Control => tool.name != "tyde_await_agents",
            AgentControlMcpSurface::Await => tool.name == "tyde_await_agents",
        });
        let tiers_enabled = self
            .host
            .read_settings()
            .await
            .map(|settings| settings.complexity_tiers_enabled)
            .unwrap_or(false);
        if !tiers_enabled {
            for tool in &mut tools {
                if tool.name == "tyde_spawn_agent" {
                    let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
                    if let Some(properties) = schema
                        .get_mut("properties")
                        .and_then(|value| value.as_object_mut())
                    {
                        properties.remove("cost_hint");
                    }
                }
            }
        }
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let allowed = match self.surface {
            AgentControlMcpSurface::Control => request.name != "tyde_await_agents",
            AgentControlMcpSurface::Await => request.name == "tyde_await_agents",
        };
        if !allowed {
            return Ok(err_text(format!(
                "tool {} is not available on this agent-control MCP endpoint",
                request.name
            )));
        }
        let context = ToolCallContext::new(self, request, context);
        self.tool_router.call(context).await
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
    let credentials = AgentControlCredentialAuthority::new();
    let server_credentials = credentials.clone();

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
                let control_host = host_handle.clone();
                let control_credentials = server_credentials.clone();
                let control_service: StreamableHttpService<
                    TydeAgentControlMcpServer,
                    LocalSessionManager,
                > = StreamableHttpService::new(
                    move || {
                        Ok(TydeAgentControlMcpServer::new(
                            control_host.clone(),
                            control_credentials.clone(),
                            AgentControlMcpSurface::Control,
                        ))
                    },
                    Default::default(),
                    StreamableHttpServerConfig {
                        stateful_mode: false,
                        ..Default::default()
                    },
                );
                let await_credentials = server_credentials.clone();
                let await_service: StreamableHttpService<
                    TydeAgentControlMcpServer,
                    LocalSessionManager,
                > = StreamableHttpService::new(
                    move || {
                        Ok(TydeAgentControlMcpServer::new(
                            host_handle.clone(),
                            await_credentials.clone(),
                            AgentControlMcpSurface::Await,
                        ))
                    },
                    Default::default(),
                    StreamableHttpServerConfig {
                        stateful_mode: false,
                        ..Default::default()
                    },
                );
                let router = Router::new()
                    .route("/healthz", get(healthz_handler))
                    .nest_service("/mcp", control_service)
                    .nest_service("/await", await_service);
                if let Err(err) = axum::serve(listener, router).await {
                    tracing::warn!("agent-control MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn agent-control MCP server thread: {err}"))?;

    Ok(AgentControlMcpHandle {
        url: format!("http://{local_addr}/mcp"),
        await_url: format!("http://{local_addr}/await"),
        credentials,
    })
}

async fn do_spawn_agent(
    host: &HostHandle,
    input: SpawnRequestInput,
    request_agent_id: Option<AgentId>,
) -> Result<SpawnAgentResult, String> {
    reject_mutating_tool_for_read_only_caller(host, request_agent_id.as_ref(), "tyde_spawn_agent")
        .await?;

    if input.workspace_roots.iter().any(|r| r.trim().is_empty()) {
        return Err("workspace_roots must not contain empty values".to_string());
    }
    if input.prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }

    let host_settings = host.read_settings().await?;
    let launch_profile_id = input
        .launch_profile_id
        .as_deref()
        .map(parse_launch_profile_id)
        .transpose()?;
    let launch_profile_backend = match launch_profile_id.as_ref() {
        Some(launch_profile_id) => Some(
            host.resolve_launch_profile(launch_profile_id)
                .await?
                .backend_kind,
        ),
        None => None,
    };
    let backend_kind = match (
        input.backend_kind.map(BackendKind::from),
        launch_profile_backend,
    ) {
        (Some(explicit), Some(profile_backend)) if explicit != profile_backend => {
            return Err(format!(
                "launch_profile_id {} targets {:?}, but backend_kind is {:?}",
                launch_profile_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                profile_backend,
                explicit
            ));
        }
        (Some(explicit), _) => explicit,
        (None, Some(profile_backend)) => profile_backend,
        (None, None) => host_settings.default_backend.ok_or_else(|| {
            "backend_kind is required because the host has no default_backend".to_string()
        })?,
    };

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
    let caller_agent_id = request_agent_id.clone();
    let parent_agent_id = match (request_agent_id, explicit_parent) {
        (Some(caller), Some(explicit)) if caller != explicit => {
            return Err("parent_agent_id must match the authenticated caller".to_string());
        }
        (Some(caller), _) => Some(caller),
        (None, Some(_)) => {
            return Err(
                "parent_agent_id requires an authenticated agent-control caller".to_owned(),
            );
        }
        (None, None) => None,
    };
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
            launch_profile_id,
            cost_hint: input.cost_hint.map(SpawnCostHint::from),
            access_mode: input
                .access_mode
                .map(BackendAccessMode::from)
                .unwrap_or_default(),
            session_settings: input.session_settings,
        },
    };

    let agent_id = host
        .spawn_agent_from_agent_control(payload, caller_agent_id.as_ref())
        .await?;
    let agent_status = host
        .agent_status_snapshot(&agent_id)
        .await
        .ok_or_else(|| format!("spawned agent {} missing status snapshot", agent_id.0))?
        .status();
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

async fn caller_project_scope(
    host: &HostHandle,
    caller: &AgentId,
) -> Result<(ProjectId, Vec<protocol::Project>), String> {
    let caller_project_id = host
        .project_id_for_agent(caller)
        .await
        .ok_or_else(|| "authenticated caller is not assigned to a project".to_owned())?;
    let projects = host.list_projects().await?;
    let caller_project = projects
        .iter()
        .find(|project| project.id == caller_project_id)
        .ok_or_else(|| format!("caller project {caller_project_id} no longer exists"))?;
    let canonical_project_id = caller_project
        .parent_project_id()
        .cloned()
        .unwrap_or_else(|| caller_project_id.clone());
    let scoped = projects
        .into_iter()
        .filter(|project| {
            project.id == canonical_project_id
                || project.parent_project_id() == Some(&canonical_project_id)
        })
        .collect();
    Ok((canonical_project_id, scoped))
}

async fn do_create_workbench(
    host: &HostHandle,
    caller: &AgentId,
    input: CreateWorkbenchToolInput,
) -> Result<CreateWorkbenchResult, String> {
    let parent_project_id = parse_project_id(&input.parent_project_id)?;
    let (canonical_project_id, _) = caller_project_scope(host, caller).await?;
    if parent_project_id != canonical_project_id {
        return Err(format!(
            "parent_project_id {} is outside caller project scope {}",
            parent_project_id, canonical_project_id
        ));
    }
    let branch = input.branch.trim();
    if branch.is_empty() {
        return Err("branch must not be empty".to_owned());
    }
    let name = match input.name {
        Some(name) if name.trim().is_empty() => {
            return Err("name must not be empty when supplied".to_owned());
        }
        Some(name) => name,
        None => branch.to_owned(),
    };
    let base = input.base_ref.map(BaseRevision);
    let created = host
        .create_workbench(
            WorkbenchCreatePayload {
                parent_project_id,
                branch: GitBranchName(branch.to_owned()),
                name,
            },
            base,
        )
        .await
        .map_err(|error| error.to_string())?;
    create_workbench_result(created)
}

fn create_workbench_result(created: CreatedWorkbench) -> Result<CreateWorkbenchResult, String> {
    let project_id = created.project.id.0;
    let name = created.project.name;
    let ProjectSource::GitWorkbench {
        parent_project_id,
        branch,
        ..
    } = created.project.source
    else {
        return Err(format!(
            "workbench_create returned standalone project {project_id}"
        ));
    };
    Ok(CreateWorkbenchResult {
        project_id,
        name,
        branch: branch.0,
        parent_project_id: parent_project_id.0,
        roots: created
            .roots
            .into_iter()
            .map(|root| CreatedWorkbenchRootResult {
                parent_root: root.root.parent_root.0,
                worktree_root: root.root.worktree_root.0,
                base_commit: root.base_commit,
                parent_root_dirty: root.parent_root_dirty,
            })
            .collect(),
    })
}

async fn do_list_workbenches(
    host: &HostHandle,
    caller: &AgentId,
) -> Result<ListWorkbenchesResult, String> {
    let (caller_project_id, projects) = caller_project_scope(host, caller).await?;
    let projects = projects
        .into_iter()
        .map(|project| {
            let workspace_roots = project
                .root_paths()
                .into_iter()
                .map(|root| root.0)
                .collect();
            let (kind, parent_project_id, branch) = match &project.source {
                ProjectSource::Standalone { .. } => (ProjectKindOutput::Standalone, None, None),
                ProjectSource::GitWorkbench {
                    parent_project_id,
                    branch,
                    ..
                } => (
                    ProjectKindOutput::Workbench,
                    Some(parent_project_id.0.clone()),
                    Some(branch.0.clone()),
                ),
            };
            ProjectOverview {
                project_id: project.id.0,
                name: project.name,
                kind,
                parent_project_id,
                branch,
                workspace_roots,
            }
        })
        .collect();
    Ok(ListWorkbenchesResult {
        caller_project_id: caller_project_id.0,
        projects,
    })
}

async fn do_list_launch_options(host: &HostHandle) -> Result<ListLaunchOptionsResult, String> {
    let (catalog, default_backend, session_schemas) = host.read_launch_options().await?;
    Ok(ListLaunchOptionsResult {
        catalog,
        default_backend,
        session_schemas,
    })
}

async fn do_send_message(
    host: &HostHandle,
    agent_id: &AgentId,
    message: String,
    request_agent_id: Option<AgentId>,
) -> Result<(), String> {
    reject_mutating_tool_for_read_only_caller(
        host,
        request_agent_id.as_ref(),
        "tyde_send_agent_message",
    )
    .await?;

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
            origin: None,
            tool_response: None,
        }))
        .await;
    if !sent {
        return Err("agent backend is closed".to_string());
    }
    Ok(())
}

async fn reject_mutating_tool_for_read_only_caller(
    host: &HostHandle,
    request_agent_id: Option<&AgentId>,
    tool_name: &'static str,
) -> Result<(), String> {
    let Some(agent_id) = request_agent_id else {
        return Ok(());
    };
    if host.agent_access_mode(agent_id).await == Some(BackendAccessMode::ReadOnly) {
        return Err(format!(
            "BackendAccessMode::ReadOnly rejects mutating MCP tool '{tool_name}'"
        ));
    }
    Ok(())
}

async fn do_team_describe(
    host: &HostHandle,
    caller_agent_id: AgentId,
) -> Result<TeamDescribeResult, String> {
    let data = host.describe_team_for_agent(caller_agent_id).await?;
    let catalog = team_preset_catalog();
    let mut members = Vec::with_capacity(data.members.len());
    for member in data.members {
        let profile = describe_member_profile(member.profile.as_ref(), &catalog)?;
        let custom_agent = if let Some(custom_agent_id) = member.custom_agent_id.as_ref() {
            let custom_agent =
                host.custom_agent_by_id(custom_agent_id)
                    .await?
                    .ok_or_else(|| {
                        format!(
                            "team member {} references missing custom agent {}",
                            member.id, custom_agent_id
                        )
                    })?;
            Some(TeamCustomAgentSummary {
                id: custom_agent.id,
                name: custom_agent.name,
                description: custom_agent.description,
            })
        } else {
            None
        };
        let binding = team_describe_binding(&data.bindings, &member.id)?;
        members.push(TeamDescribeMember {
            member,
            profile,
            custom_agent,
            binding,
        });
    }
    Ok(TeamDescribeResult {
        team: data.team,
        members,
    })
}

fn describe_member_profile(
    profile: Option<&protocol::TeamMemberPresetProfile>,
    catalog: &protocol::TeamPresetCatalog,
) -> Result<Option<TeamProfileSummary>, String> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let role_preset = match profile.role_preset_id.as_ref() {
        Some(role_preset_id) => Some(
            catalog
                .role_presets
                .iter()
                .find(|preset| preset.id == *role_preset_id)
                .ok_or_else(|| format!("missing role preset {role_preset_id}"))?
                .name
                .clone(),
        ),
        None => None,
    };
    let personality_preset = match profile.personality_preset_id.as_ref() {
        Some(personality_preset_id) => Some(
            catalog
                .personality_presets
                .iter()
                .find(|preset| preset.id == *personality_preset_id)
                .ok_or_else(|| format!("missing personality preset {personality_preset_id}"))?
                .name
                .clone(),
        ),
        None => None,
    };
    let mut traits = Vec::new();
    for trait_id in &profile.personality_traits {
        let name = catalog
            .personality_traits
            .iter()
            .find(|preset| preset.trait_id == *trait_id)
            .ok_or_else(|| format!("missing personality trait {trait_id:?}"))?
            .name
            .clone();
        traits.push(name);
    }
    Ok(Some(TeamProfileSummary {
        role_preset,
        personality_preset,
        traits,
    }))
}

fn team_describe_binding(
    bindings: &[TeamMemberBindingPayload],
    member_id: &TeamMemberId,
) -> Result<TeamMemberBindingPayload, String> {
    bindings
        .iter()
        .find(|binding| binding.member_id == *member_id)
        .cloned()
        .ok_or_else(|| format!("team member {member_id} has no team registry binding"))
}

async fn do_team_message_member(
    host: &HostHandle,
    caller_agent_id: AgentId,
    input: TeamMessageMemberToolInput,
) -> Result<TeamMessageMemberResult, String> {
    let member_id = parse_team_member_id(&input.member_id)?;
    if input.message.trim().is_empty() {
        return Err("message must not be empty".to_string());
    }
    let images = input.images.map(|images| {
        images
            .into_iter()
            .map(|image| ImageData {
                media_type: image.media_type,
                data: image.data,
            })
            .collect::<Vec<_>>()
    });
    if let Some(images) = images.as_ref() {
        for image in images {
            if image.media_type.trim().is_empty() {
                return Err("images media_type must not be empty".to_string());
            }
            if image.data.trim().is_empty() {
                return Err("images data must not be empty".to_string());
            }
        }
    }
    let outcome = host
        .message_team_member(caller_agent_id, member_id, input.message, images)
        .await?;
    Ok(TeamMessageMemberResult {
        member_id: outcome.member_id.0,
        agent_id: outcome.agent_id.0,
        queued: outcome.queued,
    })
}

async fn do_workflow_targets(
    host: &HostHandle,
    caller_agent_id: Option<&AgentId>,
) -> Result<WorkflowTargetsResponse, String> {
    host.workflow_targets_for_agent(caller_agent_id).await
}

async fn do_workflow_save(
    host: &HostHandle,
    input: WorkflowSaveRequest,
) -> Result<WorkflowSaveResponse, String> {
    host.workflow_save_from_agent(input).await
}

async fn do_list_agents(
    host: &HostHandle,
    caller_agent_id: &AgentId,
) -> Result<Vec<AgentOverview>, String> {
    if host.agent_handle(caller_agent_id).await.is_none() {
        return Err(format!("unknown caller agent_id {}", caller_agent_id.0));
    }
    let agents = host
        .list_agents()
        .await
        .into_iter()
        .filter(|start| start.parent_agent_id.as_ref() == Some(caller_agent_id))
        .collect::<Vec<_>>();
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
    context: RequestContext<RoleServer>,
) -> Result<AwaitAgentsResult, String> {
    let cancellation_token = context.ct.clone();
    let progress_reporter = AwaitProgressReporter::from_context(&context);
    do_await_agents_with_progress(host, agent_ids, Some(cancellation_token), progress_reporter)
        .await
}

async fn do_await_agents_with_progress(
    host: &HostHandle,
    agent_ids: Vec<AgentId>,
    cancellation_token: Option<CancellationToken>,
    progress_reporter: Option<AwaitProgressReporter>,
) -> Result<AwaitAgentsResult, String> {
    if agent_ids.is_empty() {
        return Err("agent_ids must contain at least one agent_id".to_string());
    }

    for agent_id in &agent_ids {
        if host.agent_status_snapshot(agent_id).await.is_none() {
            return Err(format!("unknown agent_id {}", agent_id.0));
        }
    }

    let mut status_rx = host.subscribe_agent_status_changes().await;
    let progress_interval = progress_reporter
        .as_ref()
        .map(|reporter| reporter.interval)
        .unwrap_or(AWAIT_TOOL_PROGRESS_INTERVAL);
    let mut progress_tick =
        tokio::time::interval_at(Instant::now() + progress_interval, progress_interval);
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut progress_count = 0.0;
    let mut emitted_initial_progress = false;

    loop {
        let result = await_result_from_snapshot(host, &agent_ids).await?;
        if !result.ready.is_empty() || result.still_thinking.is_empty() {
            return Ok(result);
        }
        if let Some(progress_reporter) = progress_reporter.as_ref()
            && !emitted_initial_progress
        {
            progress_count += 1.0;
            progress_reporter
                .notify(progress_count, result.still_thinking.len())
                .await;
            emitted_initial_progress = true;
        }

        tokio::select! {
            changed = status_rx.changed() => {
                if changed.is_err() {
                    return Err("agent status notification channel closed".to_string());
                }
            }
            _ = progress_tick.tick(), if progress_reporter.is_some() => {
                let result = await_result_from_snapshot(host, &agent_ids).await?;
                if !result.ready.is_empty() || result.still_thinking.is_empty() {
                    return Ok(result);
                }
                if let Some(progress_reporter) = progress_reporter.as_ref() {
                    progress_count += 1.0;
                    progress_reporter
                        .notify(progress_count, result.still_thinking.len())
                        .await;
                }
            }
            _ = async {
                if let Some(cancellation_token) = cancellation_token.as_ref() {
                    cancellation_token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                return await_result_from_snapshot(host, &agent_ids).await;
            }
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
        if status.is_plan_approval_pending() || !status.is_active() {
            ready.push(entry);
        } else {
            still_thinking.push(entry);
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
) -> Result<AgentControlReadResult, String> {
    let handle = host
        .agent_handle(agent_id)
        .await
        .ok_or_else(|| format!("unknown agent_id {}", agent_id.0))?;
    let latest = handle
        .read_latest_output()
        .await
        .ok_or_else(|| format!("agent {} is not available", agent_id.0))??;

    Ok(AgentControlReadResult {
        agent_id: agent_id.clone(),
        output: latest,
    })
}

async fn do_read_agent_debug(
    host: &HostHandle,
    agent_id: &AgentId,
    after_seq: Option<u64>,
    limit: Option<u32>,
    max_bytes: Option<u32>,
) -> Result<AgentControlReadDebugResult, String> {
    let limit = limit
        .map(|value| value as usize)
        .unwrap_or(AGENT_CONTROL_DEFAULT_READ_LIMIT);
    if limit == 0 {
        return Err("limit must be greater than zero".to_string());
    }
    if limit > AGENT_CONTROL_MAX_READ_LIMIT {
        return Err(format!("limit must be <= {AGENT_CONTROL_MAX_READ_LIMIT}"));
    }
    let max_bytes = max_bytes
        .map(|value| value as usize)
        .unwrap_or(AGENT_CONTROL_DEFAULT_READ_MAX_BYTES);
    if max_bytes == 0 {
        return Err("max_bytes must be greater than zero".to_string());
    }
    if max_bytes > AGENT_CONTROL_MAX_READ_MAX_BYTES {
        return Err(format!(
            "max_bytes must be <= {AGENT_CONTROL_MAX_READ_MAX_BYTES}"
        ));
    }

    let handle = host
        .agent_handle(agent_id)
        .await
        .ok_or_else(|| format!("unknown agent_id {}", agent_id.0))?;
    let events = handle
        .read_output(after_seq, limit)
        .await
        .ok_or_else(|| format!("agent {} is not available", agent_id.0))?;
    let capped = cap_agent_control_events(events, max_bytes, after_seq)
        .map_err(|error| format!("failed to size agent output events: {error}"))?;

    Ok(AgentControlReadDebugResult {
        agent_id: agent_id.clone(),
        events: capped.events,
        next_after_seq: capped.next_after_seq,
        max_bytes,
        omitted_events: capped.omitted_events,
        omitted_event_bytes: capped.omitted_event_bytes,
    })
}

#[derive(Debug)]
struct SpawnRequestInput {
    workspace_roots: Vec<String>,
    prompt: String,
    launch_profile_id: Option<String>,
    backend_kind: Option<BackendKindInput>,
    session_settings: Option<SessionSettingsValues>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
    access_mode: Option<BackendAccessModeInput>,
}

impl From<SpawnAgentToolInput> for SpawnRequestInput {
    fn from(v: SpawnAgentToolInput) -> Self {
        Self {
            workspace_roots: v.workspace_roots,
            prompt: v.prompt,
            launch_profile_id: v.launch_profile_id,
            backend_kind: v.backend_kind,
            session_settings: v.session_settings,
            parent_agent_id: v.parent_agent_id,
            project_id: v.project_id,
            name: v.name,
            cost_hint: v.cost_hint,
            access_mode: v.access_mode,
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

fn parse_launch_profile_id(input: &str) -> Result<LaunchProfileId, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("launch_profile_id must not be empty".to_string());
    }
    Ok(LaunchProfileId(trimmed.to_owned()))
}

fn parse_team_member_id(input: &str) -> Result<TeamMemberId, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("member_id must not be empty".to_string());
    }
    Ok(TeamMemberId(trimmed.to_string()))
}

fn split_request_target(target: &str) -> Result<(&str, Option<AgentId>), String> {
    let path = target.split('?').next().unwrap_or(target);
    let Some((_, query)) = target.split_once('?') else {
        return Ok((path, None));
    };
    Ok((path, parse_agent_id_from_query(query)?))
}

fn parse_agent_id_from_query(query: &str) -> Result<Option<AgentId>, String> {
    let mut parsed = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "agent_id" {
            continue;
        }
        if parsed.is_some() {
            return Err("agent_id query parameter must not be repeated".to_owned());
        }
        let decoded = percent_decode_query_component(value)
            .ok_or_else(|| format!("invalid agent_id query parameter encoding: {value}"))?;
        parsed = Some(parse_agent_id(&decoded)?);
    }
    Ok(parsed)
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
    use protocol::{
        AgentControlOutput, AgentErrorCode, AgentErrorPayload, ChatEvent, ChatMessage, Envelope,
        FrameKind, MessageSender, ReasoningData, StreamPath, ToolUseData,
    };
    use serde_json::Value;
    use tokio::time::{sleep, timeout};

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

    #[test]
    fn caller_credentials_bind_signature_to_agent_id() {
        let credentials = AgentControlCredentialAuthority::new();
        let caller = AgentId("550e8400-e29b-41d4-a716-446655440000".to_owned());
        let other = "550e8400-e29b-41d4-a716-446655440001";
        let token = credentials.issue(&caller);
        assert_eq!(
            credentials.verify(&token).expect("valid credential"),
            caller
        );

        let (_, _, signature) = token
            .split_once('.')
            .and_then(|(version, rest)| {
                rest.rsplit_once('.')
                    .map(|(agent_id, signature)| (version, agent_id, signature))
            })
            .expect("credential fields");
        let spoofed = format!("v1.{other}.{signature}");
        assert!(credentials.verify(&spoofed).is_err());
    }

    #[test]
    fn cap_read_events_advances_past_omitted_events() {
        let events = vec![
            Envelope::from_payload(
                StreamPath("/agent/a".to_owned()),
                protocol::FrameKind::ChatEvent,
                1,
                &serde_json::json!({"text": "small"}),
            )
            .expect("small event"),
            Envelope::from_payload(
                StreamPath("/agent/a".to_owned()),
                protocol::FrameKind::ChatEvent,
                2,
                &serde_json::json!({"text": "x".repeat(4096)}),
            )
            .expect("large event"),
        ];

        let capped = cap_agent_control_events(events, 512, None)
            .expect("typed envelopes should serialize for byte capping");

        assert_eq!(capped.events.len(), 1);
        assert_eq!(capped.events[0].seq, 1);
        assert_eq!(capped.next_after_seq, Some(2));
        assert_eq!(capped.omitted_events, 1);
        assert!(capped.omitted_event_bytes > 512);
    }

    fn assistant_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 1,
            sender: MessageSender::Assistant {
                agent: "worker".to_owned(),
            },
            content: content.to_owned(),
            reasoning: Some(ReasoningData {
                text: "private reasoning".to_owned(),
                tokens: None,
                signature: None,
                blob: None,
            }),
            tool_calls: vec![ToolUseData {
                id: "tool-1".to_owned(),
                name: "private_tool".to_owned(),
                arguments: json!({"private": true}),
            }],
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn output_envelope(seq: u64, event: &ChatEvent) -> Envelope {
        Envelope::from_payload(
            StreamPath("/agent/worker".to_owned()),
            FrameKind::ChatEvent,
            seq,
            event,
        )
        .expect("output envelope")
    }

    #[test]
    fn latest_agent_output_returns_only_visible_message_text() {
        let output = protocol::agent_control_output_from_envelope(&output_envelope(
            1,
            &ChatEvent::MessageAdded(assistant_message("visible answer")),
        ))
        .expect("project latest output")
        .expect("assistant message is an output record");

        assert_eq!(
            output,
            AgentControlOutput::Message {
                text: "visible answer".to_owned()
            }
        );
        let encoded = serde_json::to_value(output).expect("serialize output");
        assert!(encoded.get("reasoning").is_none());
        assert!(encoded.get("tool_calls").is_none());
        assert!(encoded.get("metadata").is_none());
    }

    #[test]
    fn latest_agent_output_preserves_empty_and_error_records() {
        assert_eq!(AgentControlOutput::default(), AgentControlOutput::Empty);
        assert_eq!(
            protocol::agent_control_output_from_envelope(&output_envelope(
                2,
                &ChatEvent::MessageAdded(assistant_message("")),
            ))
            .expect("empty latest message")
            .expect("empty assistant message is an output record"),
            AgentControlOutput::Empty
        );

        let error = AgentErrorPayload {
            agent_id: AgentId("550e8400-e29b-41d4-a716-446655440000".to_owned()),
            code: AgentErrorCode::BackendFailed,
            message: "backend failed".to_owned(),
            fatal: true,
        };
        let envelope = Envelope::from_payload(
            StreamPath("/agent/worker".to_owned()),
            FrameKind::AgentError,
            3,
            &error,
        )
        .expect("error envelope");
        assert_eq!(
            protocol::agent_control_output_from_envelope(&envelope)
                .expect("error output")
                .expect("agent error is an output record"),
            AgentControlOutput::Error { error }
        );
    }

    fn input_schema<T: schemars::JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).expect("serialize input schema")
    }

    #[test]
    fn read_tool_schemas_separate_latest_and_debug_inputs() {
        let tools = TydeAgentControlMcpServer::tool_router().list_all();
        assert!(tools.iter().any(|tool| tool.name == "tyde_read_agent"));
        assert!(
            tools
                .iter()
                .any(|tool| tool.name == "tyde_read_agent_debug")
        );

        let latest = input_schema::<ReadAgentToolInput>();
        assert_eq!(
            latest.get("additionalProperties"),
            Some(&Value::Bool(false))
        );
        assert!(latest.pointer("/properties/agent_id").is_some());
        for field in ["after_seq", "limit", "max_bytes"] {
            assert!(latest.pointer(&format!("/properties/{field}")).is_none());
        }

        let debug = input_schema::<ReadAgentDebugToolInput>();
        for field in ["agent_id", "after_seq", "limit", "max_bytes"] {
            assert!(debug.pointer(&format!("/properties/{field}")).is_some());
        }
    }

    #[test]
    fn latest_and_await_inputs_reject_legacy_fields() {
        let await_schema = input_schema::<AwaitAgentsToolInput>();
        assert_eq!(
            await_schema.pointer("/properties/agent_ids/minItems"),
            Some(&json!(1))
        );
        assert_eq!(
            await_schema.pointer("/properties/agent_ids/items/minLength"),
            Some(&json!(1))
        );
        for field in ["after_seq", "limit", "max_bytes"] {
            let mut input = serde_json::Map::from_iter([(
                "agent_id".to_owned(),
                json!("550e8400-e29b-41d4-a716-446655440000"),
            )]);
            input.insert(field.to_owned(), json!(1));
            let err = serde_json::from_value::<ReadAgentToolInput>(Value::Object(input))
                .expect_err("latest read must reject debug fields");
            assert!(err.to_string().contains("unknown field") && err.to_string().contains(field));
        }
        for field in ["timeout", "timeout_ms"] {
            let mut input = serde_json::Map::from_iter([(
                "agent_ids".to_owned(),
                json!(["550e8400-e29b-41d4-a716-446655440000"]),
            )]);
            input.insert(field.to_owned(), json!(1));
            let err = serde_json::from_value::<AwaitAgentsToolInput>(Value::Object(input))
                .expect_err("await must reject timeout fields");
            assert!(err.to_string().contains("unknown field") && err.to_string().contains(field));
        }
    }

    #[test]
    fn send_message_schema_requires_non_empty_fields() {
        let schema = input_schema::<SendAgentMessageToolInput>();
        assert_eq!(
            schema.pointer("/properties/agent_id/minLength"),
            Some(&json!(1))
        );
        assert_eq!(
            schema.pointer("/properties/message/minLength"),
            Some(&json!(1))
        );
        assert_eq!(
            schema.get("required"),
            Some(&json!(["agent_id", "message"]))
        );
    }

    #[test]
    fn debug_input_accepts_incremental_controls() {
        let input = serde_json::from_value::<ReadAgentDebugToolInput>(json!({
            "agent_id": "550e8400-e29b-41d4-a716-446655440000",
            "after_seq": 7,
            "limit": 8,
            "max_bytes": 4096,
        }))
        .expect("debug input");
        assert_eq!(input.after_seq, Some(7));
        assert_eq!(input.limit, Some(8));
        assert_eq!(input.max_bytes, Some(4096));
    }

    fn hermes_claude_session_settings() -> protocol::SessionSettingsValues {
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "reasoning_effort".to_owned(),
            protocol::SessionSettingValue::String("high".to_owned()),
        );
        settings
            .0
            .insert("fast".to_owned(), protocol::SessionSettingValue::Bool(true));
        settings
    }

    fn hermes_claude_launch_profile() -> protocol::HostLaunchProfileConfig {
        protocol::HostLaunchProfileConfig {
            id: LaunchProfileId("hermes:claude".to_owned()),
            label: "Hermes: Claude".to_owned(),
            description: Some("Launch Hermes with an explicit Claude preset.".to_owned()),
            backend_kind: BackendKind::Hermes,
            session_settings: hermes_claude_session_settings(),
        }
    }

    fn mock_spawn_input(name: &str, parent_agent_id: Option<String>) -> SpawnRequestInput {
        SpawnAgentToolInput {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: format!("work for {name}"),
            launch_profile_id: None,
            backend_kind: Some(BackendKindInput::Claude),
            session_settings: None,
            parent_agent_id,
            project_id: None,
            name: Some(name.to_owned()),
            cost_hint: None,
            access_mode: None,
        }
        .into()
    }

    #[tokio::test]
    async fn list_agents_returns_only_callers_direct_children() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        let caller = do_spawn_agent(&host, mock_spawn_input("caller", None), None)
            .await
            .expect("spawn caller");
        let caller_id = AgentId(caller.agent_id);
        let child = do_spawn_agent(
            &host,
            mock_spawn_input("direct-child", None),
            Some(caller_id.clone()),
        )
        .await
        .expect("spawn direct child");
        let child_id = AgentId(child.agent_id.clone());
        let _grandchild =
            do_spawn_agent(&host, mock_spawn_input("grandchild", None), Some(child_id))
                .await
                .expect("spawn grandchild");
        let _unrelated = do_spawn_agent(&host, mock_spawn_input("unrelated", None), None)
            .await
            .expect("spawn unrelated agent");

        let listed = do_list_agents(&host, &caller_id)
            .await
            .expect("list caller children");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent_id, child.agent_id);
        assert_eq!(
            listed[0].parent_agent_id.as_deref(),
            Some(caller_id.0.as_str())
        );
    }

    #[tokio::test]
    async fn caller_cannot_assign_a_different_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        let caller = do_spawn_agent(&host, mock_spawn_input("caller", None), None)
            .await
            .expect("spawn caller");
        let other = do_spawn_agent(&host, mock_spawn_input("other", None), None)
            .await
            .expect("spawn other");

        let err = do_spawn_agent(
            &host,
            mock_spawn_input("spoofed", Some(other.agent_id)),
            Some(AgentId(caller.agent_id)),
        )
        .await
        .expect_err("authenticated caller must own parent assignment");
        assert!(err.contains("must match the authenticated caller"));
    }

    #[tokio::test]
    async fn spawn_agent_accepts_explicit_hermes_launch_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        host.set_setting(protocol::SetSettingPayload {
            setting: protocol::HostSettingValue::LaunchProfiles {
                profiles: vec![hermes_claude_launch_profile()],
            },
        })
        .await
        .expect("configure Hermes launch profile");
        host.set_setting(protocol::SetSettingPayload {
            setting: protocol::HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Hermes],
            },
        })
        .await
        .expect("enable Hermes");
        host.refresh_session_schemas().await;

        let options = do_list_launch_options(&host)
            .await
            .expect("list launch options");
        let profile = options
            .catalog
            .entries
            .iter()
            .find_map(|entry| match entry {
                protocol::LaunchProfileEntry::Ready { profile }
                    if profile.id.0 == "hermes:claude" =>
                {
                    Some(profile)
                }
                _ => None,
            })
            .expect("ready hermes:claude profile");
        assert_eq!(profile.backend_kind, BackendKind::Hermes);
        assert_eq!(profile.session_settings, hermes_claude_session_settings());

        let spawned = do_spawn_agent(
            &host,
            SpawnAgentToolInput {
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                prompt: "explicit Hermes profile via MCP core".to_owned(),
                launch_profile_id: Some("hermes:claude".to_owned()),
                backend_kind: None,
                session_settings: None,
                parent_agent_id: None,
                project_id: None,
                name: Some("hermes-profile".to_owned()),
                cost_hint: None,
                access_mode: None,
            }
            .into(),
            None,
        )
        .await
        .expect("spawn Hermes profile agent");
        let result =
            do_await_agents_with_progress(&host, vec![AgentId(spawned.agent_id)], None, None)
                .await
                .expect("await Hermes profile agent");
        assert!(result.still_thinking.is_empty());
        assert_eq!(result.ready.len(), 1);
        assert_eq!(result.ready[0].status, AgentControlStatus::Idle);
    }

    #[tokio::test]
    async fn await_agents_does_not_return_while_still_thinking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        let spawned = do_spawn_agent(
            &host,
            SpawnAgentToolInput {
                workspace_roots: vec!["/tmp/test".to_string()],
                prompt: "__mock_hold_until_interrupt__ keep waiting".to_string(),
                launch_profile_id: None,
                backend_kind: Some(BackendKindInput::Claude),
                session_settings: None,
                parent_agent_id: None,
                project_id: None,
                name: Some("held-agent".to_string()),
                cost_hint: None,
                access_mode: None,
            }
            .into(),
            None,
        )
        .await
        .expect("spawn held agent");
        let cancellation_token = CancellationToken::new();
        let await_future = do_await_agents_with_progress(
            &host,
            vec![AgentId(spawned.agent_id)],
            Some(cancellation_token.clone()),
            None,
        );
        tokio::pin!(await_future);

        assert!(
            timeout(Duration::from_millis(50), &mut await_future)
                .await
                .is_err(),
            "await should not return a still_thinking snapshot before an agent is ready"
        );

        cancellation_token.cancel();
        let result = timeout(Duration::from_secs(1), &mut await_future)
            .await
            .expect("await should finish after cancellation")
            .expect("await should return a cancellation snapshot");
        assert!(result.ready.is_empty());
        assert_eq!(result.still_thinking.len(), 1);
        assert_eq!(
            result.still_thinking[0].status,
            AgentControlStatus::Thinking
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_agents_remains_pending_beyond_prior_300_second_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        let spawned = do_spawn_agent(
            &host,
            SpawnAgentToolInput {
                workspace_roots: vec!["/tmp/test".to_string()],
                prompt: "__mock_hold_until_interrupt__ boundary".to_string(),
                launch_profile_id: None,
                backend_kind: Some(BackendKindInput::Claude),
                session_settings: None,
                parent_agent_id: None,
                project_id: None,
                name: Some("boundary-agent".to_string()),
                cost_hint: None,
                access_mode: None,
            }
            .into(),
            None,
        )
        .await
        .expect("spawn held agent");
        let cancellation_token = CancellationToken::new();
        let await_future = do_await_agents_with_progress(
            &host,
            vec![AgentId(spawned.agent_id)],
            Some(cancellation_token.clone()),
            None,
        );
        tokio::pin!(await_future);

        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::task::yield_now().await;
        assert!(
            timeout(Duration::ZERO, &mut await_future).await.is_err(),
            "await must remain pending beyond the former client boundary"
        );

        cancellation_token.cancel();
        let result = await_future
            .await
            .expect("cancellation should return a status snapshot");
        assert_eq!(result.still_thinking.len(), 1);
    }

    #[tokio::test]
    async fn await_agents_returns_snapshot_on_request_cancellation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("mock host");
        let spawned = do_spawn_agent(
            &host,
            SpawnAgentToolInput {
                workspace_roots: vec!["/tmp/test".to_string()],
                prompt: crate::backend::mock::MOCK_SLOW_TURN_SENTINEL.to_string(),
                launch_profile_id: None,
                backend_kind: Some(BackendKindInput::Claude),
                session_settings: None,
                parent_agent_id: None,
                project_id: None,
                name: Some("cancel-agent".to_string()),
                cost_hint: None,
                access_mode: None,
            }
            .into(),
            None,
        )
        .await
        .expect("spawn slow agent");
        let cancellation_token = CancellationToken::new();
        let cancel_task_token = cancellation_token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            cancel_task_token.cancel();
        });

        let result = do_await_agents_with_progress(
            &host,
            vec![AgentId(spawned.agent_id)],
            Some(cancellation_token),
            None,
        )
        .await
        .expect("await should return a status snapshot on cancellation");

        assert!(result.ready.is_empty());
        assert_eq!(result.still_thinking.len(), 1);
        assert_eq!(
            result.still_thinking[0].status,
            AgentControlStatus::Thinking
        );
    }

    #[test]
    fn team_describe_binding_rejects_missing_member_binding() {
        let member_id = TeamMemberId("member-without-binding".to_owned());
        let err =
            team_describe_binding(&[], &member_id).expect_err("missing binding should be surfaced");
        assert!(
            err.contains("team member member-without-binding has no team registry binding"),
            "unexpected missing-binding error: {err}"
        );
    }
}
