//! Built-in `tyde-config` MCP server.
//!
//! Exposes host configuration — settings, custom agents, skills, MCP servers,
//! backend setup status — as MCP tools. Attached only to spawns of the
//! builtin Help agent so a user can ask it to inspect and change Tyde
//! configuration directly. All mutations go through the same `HostHandle`
//! methods the protocol handlers use, so connected clients see changes
//! immediately via the usual notify fan-out.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::{Json, Router, response::IntoResponse, routing::get};
use protocol::{
    BackendKind, CodeIntelProviderId, CustomAgent, CustomAgentDeletePayload, CustomAgentId,
    CustomAgentUpsertPayload, HostExecutablePath, HostSettingValue, McpServerConfig,
    McpServerDeletePayload, McpServerId, McpServerUpsertPayload, McpTransportConfig,
    SetSettingPayload, Skill, SkillId, SkillRefreshPayload, ToolPolicy,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
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
use uuid::Uuid;

use crate::backend::setup;
use crate::host::HostHandle;

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";

#[derive(Clone, Debug)]
pub struct ConfigMcpHandle {
    pub url: String,
}

#[derive(Clone)]
struct TydeConfigMcpServer {
    host: HostHandle,
    tool_router: ToolRouter<Self>,
}

impl TydeConfigMcpServer {
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "setting", rename_all = "snake_case", deny_unknown_fields)]
enum SettingInput {
    /// Replace the set of enabled backends.
    EnabledBackends {
        enabled_backends: Vec<BackendKindInput>,
    },
    /// Set (or clear, with null) the backend used when none is picked.
    DefaultBackend {
        default_backend: Option<BackendKindInput>,
    },
    /// Turn the Low/High task complexity tiers on or off.
    ComplexityTiersEnabled { enabled: bool },
    /// Expose the tyde-debug MCP server to agents.
    TydeDebugMcpEnabled { enabled: bool },
    /// Expose the tyde-agent-control MCP server to agents.
    TydeAgentControlMcpEnabled { enabled: bool },
    /// Allow paired mobile devices to connect.
    EnableMobileConnections { enabled: bool },
    /// Set (or clear, with null) a code-intelligence language-server binary path.
    CodeIntelLanguageServerPath {
        provider: String,
        path: Option<String>,
    },
}

impl From<SettingInput> for HostSettingValue {
    fn from(value: SettingInput) -> Self {
        match value {
            SettingInput::EnabledBackends { enabled_backends } => Self::EnabledBackends {
                enabled_backends: enabled_backends.into_iter().map(Into::into).collect(),
            },
            SettingInput::DefaultBackend { default_backend } => Self::DefaultBackend {
                default_backend: default_backend.map(Into::into),
            },
            SettingInput::ComplexityTiersEnabled { enabled } => {
                Self::ComplexityTiersEnabled { enabled }
            }
            SettingInput::TydeDebugMcpEnabled { enabled } => Self::TydeDebugMcpEnabled { enabled },
            SettingInput::TydeAgentControlMcpEnabled { enabled } => {
                Self::TydeAgentControlMcpEnabled { enabled }
            }
            SettingInput::EnableMobileConnections { enabled } => {
                Self::EnableMobileConnections { enabled }
            }
            SettingInput::CodeIntelLanguageServerPath { provider, path } => {
                Self::CodeIntelLanguageServerPath {
                    provider: CodeIntelProviderId(provider),
                    path: path.map(HostExecutablePath),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetSettingToolInput {
    setting: SettingInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CustomAgentIdToolInput {
    custom_agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpsertCustomAgentToolInput {
    /// Omit to create a new agent; pass an existing id to replace it.
    custom_agent_id: Option<String>,
    name: String,
    description: String,
    /// System-prompt style instructions; omit for no customization.
    instructions: Option<String>,
    /// Skill ids to attach (see tyde_config_list_skills).
    skill_ids: Option<Vec<String>>,
    /// MCP server ids to attach (see tyde_config_list_mcp_servers).
    mcp_server_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpsertSkillToolInput {
    /// Omit to create an id from the skill name.
    skill_id: Option<String>,
    /// Directory name under ~/.tyde/skills; keep it slug-like.
    name: String,
    title: Option<String>,
    description: Option<String>,
    /// Full SKILL.md body.
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillIdToolInput {
    skill_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum McpTransportInput {
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        args: Option<Vec<String>>,
        env: Option<HashMap<String, String>>,
    },
}

impl From<McpTransportInput> for McpTransportConfig {
    fn from(value: McpTransportInput) -> Self {
        match value {
            McpTransportInput::Http {
                url,
                headers,
                bearer_token_env_var,
            } => Self::Http {
                url,
                headers: headers.unwrap_or_default(),
                bearer_token_env_var,
            },
            McpTransportInput::Stdio { command, args, env } => Self::Stdio {
                command,
                args: args.unwrap_or_default(),
                env: env.unwrap_or_default(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpsertMcpServerToolInput {
    /// Omit to create a new id.
    mcp_server_id: Option<String>,
    /// Unique MCP server name exposed to agents.
    name: String,
    transport: McpTransportInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpServerIdToolInput {
    mcp_server_id: String,
}

#[derive(Debug, Serialize)]
struct CustomAgentSummary {
    custom_agent_id: String,
    name: String,
    description: String,
    has_instructions: bool,
    skill_ids: Vec<String>,
    mcp_server_ids: Vec<String>,
}

fn summarize(agent: &CustomAgent) -> CustomAgentSummary {
    CustomAgentSummary {
        custom_agent_id: agent.id.0.clone(),
        name: agent.name.clone(),
        description: agent.description.clone(),
        has_instructions: agent.instructions.is_some(),
        skill_ids: agent.skill_ids.iter().map(|id| id.0.clone()).collect(),
        mcp_server_ids: agent.mcp_server_ids.iter().map(|id| id.0.clone()).collect(),
    }
}

fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::json(value)?]))
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

#[tool_router]
impl TydeConfigMcpServer {
    #[tool(description = "Read the current Tyde host settings.")]
    async fn tyde_config_get_settings(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self.host.read_settings().await {
            Ok(settings) => ok_json(settings),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Change one Tyde host setting. Connected clients see the change immediately."
    )]
    async fn tyde_config_set_setting(
        &self,
        Parameters(input): Parameters<SetSettingToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .host
            .set_setting(SetSettingPayload {
                setting: input.setting.into(),
            })
            .await;
        match result {
            Ok(()) => match self.host.read_settings().await {
                Ok(settings) => ok_json(settings),
                Err(err) => Ok(err_text(err)),
            },
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(
        description = "Report each backend's setup status: installed or not, version, and docs URL. Installing and signing in must be done by the user from Settings → Backends (sign-in runs the CLI's own flow in the dock terminal)."
    )]
    async fn tyde_config_backend_status(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let payload = setup::collect_backend_setup().await;
        let backends: Vec<_> = payload
            .backends
            .iter()
            .map(|info| {
                json!({
                    "backend_kind": info.backend_kind,
                    "status": info.status,
                    "installed_version": info.installed_version,
                    "docs_url": info.docs_url,
                })
            })
            .collect();
        ok_json(backends)
    }

    #[tool(description = "List all custom agents (id, name, description, attachments).")]
    async fn tyde_config_list_custom_agents(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self.host.list_custom_agents().await {
            Ok(agents) => ok_json(agents.iter().map(summarize).collect::<Vec<_>>()),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Read one custom agent in full, including its instructions.")]
    async fn tyde_config_get_custom_agent(
        &self,
        Parameters(input): Parameters<CustomAgentIdToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .host
            .custom_agent_by_id(&CustomAgentId(input.custom_agent_id.clone()))
            .await
        {
            Ok(Some(agent)) => ok_json(agent),
            Ok(None) => Ok(err_text(format!(
                "no custom agent with id {}",
                input.custom_agent_id
            ))),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Create or replace a custom agent. Omit custom_agent_id to create. Replacing overwrites the whole record, so read it first with tyde_config_get_custom_agent and resend unchanged fields."
    )]
    async fn tyde_config_upsert_custom_agent(
        &self,
        Parameters(input): Parameters<UpsertCustomAgentToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let id = input
            .custom_agent_id
            .unwrap_or_else(|| format!("ca-{}", Uuid::new_v4().simple()));
        // Preserve fields this tool doesn't model (MCP servers, tool policy)
        // when replacing an existing record.
        let existing = match self
            .host
            .custom_agent_by_id(&CustomAgentId(id.clone()))
            .await
        {
            Ok(existing) => existing,
            Err(err) => return Ok(err_text(err)),
        };
        let custom_agent = CustomAgent {
            id: CustomAgentId(id),
            name: input.name,
            description: input.description,
            instructions: input.instructions,
            skill_ids: input
                .skill_ids
                .map(|ids| ids.into_iter().map(SkillId).collect())
                .or_else(|| existing.as_ref().map(|agent| agent.skill_ids.clone()))
                .unwrap_or_default(),
            mcp_server_ids: input
                .mcp_server_ids
                .map(|ids| ids.into_iter().map(McpServerId).collect())
                .or_else(|| existing.as_ref().map(|agent| agent.mcp_server_ids.clone()))
                .unwrap_or_default(),
            tool_policy: existing
                .as_ref()
                .map(|agent| agent.tool_policy.clone())
                .unwrap_or(ToolPolicy::Unrestricted),
        };
        match self
            .host
            .upsert_custom_agent(CustomAgentUpsertPayload {
                custom_agent: custom_agent.clone(),
            })
            .await
        {
            Ok(()) => ok_json(summarize(&custom_agent)),
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(
        description = "Delete a custom agent. Fails if a team role preset or team member still uses it."
    )]
    async fn tyde_config_delete_custom_agent(
        &self,
        Parameters(input): Parameters<CustomAgentIdToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .host
            .delete_custom_agent(CustomAgentDeletePayload {
                id: CustomAgentId(input.custom_agent_id.clone()),
            })
            .await
        {
            Ok(()) => ok_json(json!({ "deleted": input.custom_agent_id })),
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(description = "List available skills (id, name, title, description).")]
    async fn tyde_config_list_skills(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self.host.list_skills().await {
            Ok(skills) => ok_json(skills),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Refresh Tyde's filesystem skill index from ~/.tyde/skills.")]
    async fn tyde_config_refresh_skills(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .host
            .refresh_skills(SkillRefreshPayload::default())
            .await
        {
            Ok(()) => match self.host.list_skills().await {
                Ok(skills) => ok_json(skills),
                Err(err) => Ok(err_text(err)),
            },
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(
        description = "Install or replace a Tyde skill by writing ~/.tyde/skills/<name>/metadata.json and SKILL.md. Default agents load all Tyde skills automatically."
    )]
    async fn tyde_config_upsert_skill(
        &self,
        Parameters(input): Parameters<UpsertSkillToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let name = input.name;
        let id = input.skill_id.unwrap_or_else(|| name.clone());
        let skill = Skill {
            id: SkillId(id),
            name,
            title: input.title,
            description: input.description,
        };
        match self.host.upsert_skill(skill.clone(), input.body).await {
            Ok(()) => ok_json(skill),
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(description = "Delete a Tyde skill from ~/.tyde/skills and notify clients.")]
    async fn tyde_config_delete_skill(
        &self,
        Parameters(input): Parameters<SkillIdToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .host
            .delete_skill(SkillId(input.skill_id.clone()))
            .await
        {
            Ok(()) => ok_json(json!({ "deleted": input.skill_id })),
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(
        description = "List configured MCP servers (id, name, transport kind — no credentials)."
    )]
    async fn tyde_config_list_mcp_servers(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self.host.list_mcp_servers().await {
            Ok(servers) => {
                let summaries: Vec<_> = servers
                    .iter()
                    .map(|server| {
                        json!({
                            "mcp_server_id": server.id.0,
                            "name": server.name,
                            "transport": match &server.transport {
                                McpTransportConfig::Http { .. } => "http",
                                McpTransportConfig::Stdio { .. } => "stdio",
                            },
                        })
                    })
                    .collect();
                ok_json(summaries)
            }
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Install or replace a Tyde MCP server. Default agents load all configured MCP servers automatically."
    )]
    async fn tyde_config_upsert_mcp_server(
        &self,
        Parameters(input): Parameters<UpsertMcpServerToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let id = input
            .mcp_server_id
            .unwrap_or_else(|| format!("mcp-{}", Uuid::new_v4().simple()));
        let mcp_server = McpServerConfig {
            id: McpServerId(id),
            name: input.name,
            transport: input.transport.into(),
        };
        match self
            .host
            .upsert_mcp_server(McpServerUpsertPayload {
                mcp_server: mcp_server.clone(),
            })
            .await
        {
            Ok(()) => ok_json(mcp_server),
            Err(err) => Ok(err_text(err.message)),
        }
    }

    #[tool(description = "Delete a configured Tyde MCP server.")]
    async fn tyde_config_delete_mcp_server(
        &self,
        Parameters(input): Parameters<McpServerIdToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .host
            .delete_mcp_server(McpServerDeletePayload {
                id: McpServerId(input.mcp_server_id.clone()),
            })
            .await
        {
            Ok(()) => ok_json(json!({ "deleted": input.mcp_server_id })),
            Err(err) => Ok(err_text(err.message)),
        }
    }
}

impl ServerHandler for TydeConfigMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for inspecting and configuring this Tyde host: read/change settings, manage custom agents, install/update/delete skills and MCP servers, and check backend setup status. Read current state before changing it, and tell the user exactly what changed."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let context = ToolCallContext::new(self, request, context);
        self.tool_router.call(context).await
    }
}

async fn healthz_handler() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

pub fn start_server(
    bind_addr: Option<SocketAddr>,
    host_handle: HostHandle,
) -> Result<ConfigMcpHandle, String> {
    let bind_addr = bind_addr.unwrap_or_else(|| {
        DEFAULT_BIND_ADDR
            .parse()
            .expect("default loopback config MCP bind addr must parse")
    });
    if !bind_addr.ip().is_loopback() {
        return Err(format!(
            "config MCP server must bind to loopback only, got {bind_addr}"
        ));
    }

    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind config MCP HTTP server on {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set config MCP listener nonblocking: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read config MCP listener addr: {err}"))?;

    std::thread::Builder::new()
        .name("tyde-config-mcp".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build config MCP runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failed to create tokio config MCP listener");
                let mcp_service: StreamableHttpService<TydeConfigMcpServer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(TydeConfigMcpServer::new(host_handle.clone())),
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
                    tracing::warn!("config MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn config MCP server thread: {err}"))?;

    Ok(ConfigMcpHandle {
        url: format!("http://{local_addr}/mcp"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_includes_skill_and_mcp_mutations() {
        let dir = tempfile::tempdir().expect("create temp host dir");
        let host = crate::host::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("spawn mock host");
        let server = TydeConfigMcpServer::new(host);
        let tool_names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<std::collections::HashSet<_>>();

        for name in [
            "tyde_config_refresh_skills",
            "tyde_config_upsert_skill",
            "tyde_config_delete_skill",
            "tyde_config_upsert_mcp_server",
            "tyde_config_delete_mcp_server",
        ] {
            assert!(tool_names.contains(name), "missing config MCP tool {name}");
        }
    }
}
