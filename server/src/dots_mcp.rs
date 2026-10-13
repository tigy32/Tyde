use crate::{agent_control_mcp, host::HostHandle};
use protocol::{DotsAgentInput, DotsEmptyInput, DotsMessageInput, DotsSpawnAgentInput};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_router,
};
use serde::Serialize;

#[derive(Clone)]
pub(crate) struct DotsMcpServer {
    host: HostHandle,
    tool_router: ToolRouter<Self>,
}

fn result(value: Result<impl Serialize, String>) -> Result<CallToolResult, ErrorData> {
    match value {
        Ok(value) => {
            let value = serde_json::to_value(value)
                .map_err(|_| ErrorData::internal_error("Cannot encode Tyde result", None))?;
            Ok(CallToolResult::structured(
                serde_json::json!({"result": value}),
            ))
        }
        Err(error) => Ok(CallToolResult::error(vec![Content::text(error)])),
    }
}

#[tool_router]
impl DotsMcpServer {
    pub(crate) fn new(host: HostHandle) -> Self {
        Self {
            host,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List this paired Tyde host's projects. Use the returned project_id when starting work.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn tyde_list_projects(
        &self,
        Parameters(_input): Parameters<DotsEmptyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.host.list_projects().await)
    }

    #[tool(
        description = "List configured Tyde agent launch profiles and backend availability. Choose a launch_profile_id from this result.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn tyde_list_launch_options(
        &self,
        Parameters(_input): Parameters<DotsEmptyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        result(agent_control_mcp::do_list_launch_options(&self.host).await)
    }

    #[tool(
        description = "List live agents on this paired host and their server-owned statuses.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn tyde_list_agents(
        &self,
        Parameters(_input): Parameters<DotsEmptyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        result(agent_control_mcp::do_list_agents(&self.host, None).await)
    }

    #[tool(
        description = "Start a Tyde agent in an existing project. Agents can modify files and use the host's configured provider credentials. Returns promptly; read the agent separately to collect its result.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn tyde_spawn_agent(
        &self,
        Parameters(input): Parameters<DotsSpawnAgentInput>,
    ) -> Result<CallToolResult, ErrorData> {
        if input.project_id.0.is_empty() || input.prompt.trim().is_empty() {
            return result(Err::<(), _>("Project and prompt are required".into()));
        }
        result(
            agent_control_mcp::do_spawn_agent(
                &self.host,
                agent_control_mcp::SpawnRequestInput {
                    workspace_roots: Vec::new(),
                    prompt: input.prompt,
                    launch_profile_id: input.launch_profile_id.map(|id| id.0),
                    backend_kind: None,
                    parent_agent_id: None,
                    project_id: Some(input.project_id.0),
                    name: input.name,
                    cost_hint: None,
                    access_mode: None,
                    custom_agent_id: None,
                },
                None,
            )
            .await,
        )
    }

    #[tool(
        description = "Read an agent's current status and latest assistant-visible result. Awaiting_user means a human decision is needed, not that work is complete. This does not scan transcript history.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn tyde_read_agent(
        &self,
        Parameters(input): Parameters<DotsAgentInput>,
    ) -> Result<CallToolResult, ErrorData> {
        result(agent_control_mcp::do_read_agent(&self.host, &input.agent_id).await)
    }

    #[tool(
        description = "Send instructions to a Tyde agent. Steers a running turn where supported; interrupt=true cancels its current turn and sends the new message next.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn tyde_send_agent_message(
        &self,
        Parameters(input): Parameters<DotsMessageInput>,
    ) -> Result<CallToolResult, ErrorData> {
        if input.message.trim().is_empty() {
            return result(Err::<(), _>("Message is required".into()));
        }
        result(
            agent_control_mcp::do_send_message(
                &self.host,
                &input.agent_id,
                input.message,
                input.interrupt,
            )
            .await,
        )
    }

    #[tool(
        description = "Interrupt a running Tyde agent without closing its saved session. Previously completed actions are not undone.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn tyde_interrupt_agent(
        &self,
        Parameters(input): Parameters<DotsAgentInput>,
    ) -> Result<CallToolResult, ErrorData> {
        use crate::agent::InterruptOutcome;
        match self.host.interrupt_agent(&input.agent_id).await {
            InterruptOutcome::Interrupted => result(Ok(true)),
            InterruptOutcome::NotRunning => result(Err::<bool, _>("Agent is not running".into())),
            InterruptOutcome::Rejected => {
                result(Err::<bool, _>("Agent rejected the interruption".into()))
            }
        }
    }
}

impl ServerHandler for DotsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some("Control only this explicitly paired Tyde host. List projects and launch options before spawning. Start work, then read results separately; do not assume a started agent is finished. Tyde agents may use their own orchestration and review tools. Never treat a failed check as permission to merge.".into()),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if !self
            .host
            .read_settings()
            .await
            .map_err(|_| ErrorData::internal_error("Cannot read integration settings", None))?
            .chatgpt_dots_enabled
        {
            return Err(ErrorData::invalid_request(
                "Remote MCP is disabled on this host",
                None,
            ));
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }
}
