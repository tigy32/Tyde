use protocol::AgentOrigin;
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
use serde::Deserialize;

use crate::agent_control_mcp::{AgentControlCredentialAuthority, authenticated_caller_from_parts};
use crate::host::HostHandle;

pub(crate) const TYCHAT_MCP_SERVER_NAME: &str = "tyde-tychat";

#[derive(Clone)]
pub(crate) struct TydeTychatMcpServer {
    host: HostHandle,
    credentials: AgentControlCredentialAuthority,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendMessageToolInput {
    /// The message to show the owner in Tychat.
    text: String,
}

#[tool_router]
impl TydeTychatMcpServer {
    #[tool(
        description = "Send a message to the owner's phone through Tychat right now, mid-turn. Use it to acknowledge a request before long work, report progress, and deliver results. If you call it during a turn, your final message for that turn is not sent."
    )]
    async fn tychat_send_message(
        &self,
        Parameters(input): Parameters<SendMessageToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = match authenticated_caller_from_parts(&self.credentials, &parts) {
            Ok(Some(caller)) => caller,
            Ok(None) => {
                return Ok(err_text(
                    "tychat_send_message requires an agent bearer credential",
                ));
            }
            Err(error) => return Ok(err_text(error)),
        };
        let Some(handle) = self.host.agent_handle(&caller).await else {
            return Ok(err_text("Calling agent is not active"));
        };
        if handle.snapshot().origin != AgentOrigin::Tychat {
            return Ok(err_text("Only the Tychat agent can message the owner"));
        }
        let text_len = input.text.len();
        match handle.send_tychat(input.text).await {
            Ok(()) => {
                tracing::info!(agent_id = %caller, text_len, "queued Tychat tool message");
                Ok(CallToolResult::success(vec![Content::text(
                    "Queued for delivery to the owner.",
                )]))
            }
            Err(error) => {
                tracing::warn!(agent_id = %caller, text_len, "Tychat tool message rejected");
                Ok(err_text(error))
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for TydeTychatMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tychat messaging for the Tychat agent. tychat_send_message delivers a message to the owner's phone immediately."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

pub(crate) fn service(
    host: HostHandle,
    credentials: AgentControlCredentialAuthority,
) -> StreamableHttpService<TydeTychatMcpServer, LocalSessionManager> {
    StreamableHttpService::new(
        move || {
            Ok(TydeTychatMcpServer {
                host: host.clone(),
                credentials: credentials.clone(),
                tool_router: TydeTychatMcpServer::tool_router(),
            })
        },
        Default::default(),
        StreamableHttpServerConfig {
            stateful_mode: false,
            ..Default::default()
        },
    )
}
