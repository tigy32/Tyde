use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{Peer, RequestContext, RoleClient, RoleServer, RunningService, ServiceExt};
use rmcp::transport::{
    StreamableHttpClientTransport, TokioChildProcess,
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

pub const DESCRIPTOR_ENV: &str = "TYDE_HERMES_MCP_DESCRIPTOR";
pub const MANAGED_SERVER_NAME: &str = "tyde";
pub const DESCRIPTOR_FILE_NAME: &str = "tyde-mcp-servers.json";
pub const READY_FILE_NAME: &str = "tyde-mcp-ready.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BridgeDescriptor {
    pub servers: Vec<BridgeServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BridgeServerConfig {
    pub name: String,
    pub transport: BridgeTransport,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeTransport {
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

#[derive(Clone)]
struct Downstream {
    name: String,
    peer: Peer<RoleClient>,
}

#[derive(Clone)]
struct HermesMcpBridge {
    downstreams: Arc<Vec<Downstream>>,
    tools: Arc<Vec<Tool>>,
    tool_owners: Arc<HashMap<String, usize>>,
    startup_error: Option<Arc<str>>,
    ready_path: Option<Arc<PathBuf>>,
}

impl ServerHandler for HermesMcpBridge {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Process-local Tyde MCP bridge. Tools are selected and authorized by the owning Tyde agent."
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
        if let Some(path) = &self.ready_path {
            let path = Arc::clone(path);
            let status = match &self.startup_error {
                Some(error) => serde_json::json!({ "ok": false, "error": error }),
                None => serde_json::json!({ "ok": true }),
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Err(error) = std::fs::write(path.as_ref(), status.to_string()) {
                    eprintln!("Tyde Hermes MCP bridge failed to publish readiness: {error}");
                }
            });
        }
        if let Some(error) = &self.startup_error {
            return Err(McpError::internal_error(error.to_string(), None));
        }
        Ok(ListToolsResult {
            tools: self.tools.as_ref().clone(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(error) = &self.startup_error {
            return Err(McpError::internal_error(error.to_string(), None));
        }
        let Some(index) = self.tool_owners.get(request.name.as_ref()).copied() else {
            return Err(McpError::invalid_params(
                format!("unknown Tyde bridge tool '{}'", request.name),
                None,
            ));
        };
        self.downstreams[index]
            .peer
            .call_tool(request)
            .await
            .map_err(|error| {
                McpError::internal_error(
                    format!(
                        "MCP server '{}' failed tool call: {error}",
                        self.downstreams[index].name
                    ),
                    None,
                )
            })
    }
}

pub async fn run() -> Result<(), String> {
    let descriptor = load_descriptor()?;
    let (mut bridge, mut clients) = build_bridge(descriptor).await;
    bridge.ready_path = std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|directory| Arc::new(directory.join(READY_FILE_NAME)));
    let service = bridge
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|error| format!("Hermes MCP bridge handshake failed: {error}"))?;
    service
        .waiting()
        .await
        .map_err(|error| format!("Hermes MCP bridge task failed: {error}"))?;
    for client in &mut clients {
        let _ = client.close_with_timeout(Duration::from_secs(1)).await;
    }
    Ok(())
}

fn load_descriptor() -> Result<Option<BridgeDescriptor>, String> {
    let path = std::env::var_os(DESCRIPTOR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("TMPDIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|directory| directory.join(DESCRIPTOR_FILE_NAME))
                .filter(|path| path.is_file())
        });
    let Some(path) = path else {
        eprintln!("Tyde Hermes MCP bridge started without a process descriptor");
        return Ok(None);
    };
    let bytes = std::fs::read(&path).map_err(|error| {
        format!(
            "failed to read Hermes MCP bridge descriptor {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_slice::<BridgeDescriptor>(&bytes)
        .map(|descriptor| {
            eprintln!(
                "Tyde Hermes MCP bridge loaded {} configured servers from {}",
                descriptor.servers.len(),
                path.display()
            );
            Some(descriptor)
        })
        .map_err(|error| format!("invalid Hermes MCP bridge descriptor: {error}"))
}

async fn build_bridge(
    descriptor: Option<BridgeDescriptor>,
) -> (HermesMcpBridge, Vec<RunningService<RoleClient, ()>>) {
    let Some(descriptor) = descriptor else {
        return (empty_bridge(None), Vec::new());
    };
    let mut downstreams = Vec::new();
    let mut clients = Vec::new();
    let mut tools = Vec::new();
    let mut tool_owners = HashMap::new();
    let mut startup_errors = Vec::new();

    for server in descriptor.servers {
        eprintln!(
            "Tyde Hermes MCP bridge connecting configured server '{}'",
            server.name
        );
        let connected = connect(&server).await;
        let client = match connected {
            Ok(client) => client,
            Err(error) => {
                startup_errors.push(format!(
                    "failed to connect configured MCP server '{}': {error}",
                    server.name
                ));
                continue;
            }
        };
        eprintln!(
            "Tyde Hermes MCP bridge connected configured server '{}'",
            server.name
        );
        let server_tools = match client.peer().list_all_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                startup_errors.push(format!(
                    "failed to list tools from MCP server '{}': {error}",
                    server.name
                ));
                continue;
            }
        };
        eprintln!(
            "Tyde Hermes MCP bridge listed {} tools from configured server '{}'",
            server_tools.len(),
            server.name
        );
        let owner = downstreams.len();
        for tool in server_tools {
            let name = tool.name.to_string();
            if tool_owners.insert(name.clone(), owner).is_some() {
                return (
                    empty_bridge(Some(format!(
                        "duplicate MCP tool name '{name}' across configured servers"
                    ))),
                    clients,
                );
            }
            tools.push(tool);
        }
        downstreams.push(Downstream {
            name: server.name,
            peer: client.peer().clone(),
        });
        clients.push(client);
    }

    if tools.is_empty() && !startup_errors.is_empty() {
        let error = startup_errors.join("; ");
        eprintln!("Tyde Hermes MCP bridge failed: {error}");
        return (empty_bridge(Some(error)), clients);
    }
    for error in startup_errors {
        eprintln!("Tyde Hermes MCP bridge warning: {error}");
    }

    (
        HermesMcpBridge {
            downstreams: Arc::new(downstreams),
            tools: Arc::new(tools),
            tool_owners: Arc::new(tool_owners),
            startup_error: None,
            ready_path: None,
        },
        clients,
    )
}

fn empty_bridge(error: Option<String>) -> HermesMcpBridge {
    HermesMcpBridge {
        downstreams: Arc::new(Vec::new()),
        tools: Arc::new(Vec::new()),
        tool_owners: Arc::new(HashMap::new()),
        startup_error: error.map(Arc::from),
        ready_path: None,
    }
}

async fn connect(server: &BridgeServerConfig) -> Result<RunningService<RoleClient, ()>, String> {
    match &server.transport {
        BridgeTransport::Http { url, headers } => {
            let mut header_map = reqwest::header::HeaderMap::new();
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| format!("invalid HTTP header name '{name}': {error}"))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|error| format!("invalid HTTP header value: {error}"))?;
                header_map.insert(name, value);
            }
            let client = reqwest::Client::builder()
                .default_headers(header_map)
                .build()
                .map_err(|error| format!("failed to build HTTP client: {error}"))?;
            let transport = StreamableHttpClientTransport::with_client(
                client,
                StreamableHttpClientTransportConfig::with_uri(url.clone()),
            );
            ().serve(transport).await.map_err(|error| error.to_string())
        }
        BridgeTransport::Stdio { command, args, env } => {
            let mut child = tokio::process::Command::new(command);
            child.args(args).envs(env);
            let transport = TokioChildProcess::new(child)
                .map_err(|error| format!("failed to spawn '{command}': {error}"))?;
            ().serve(transport).await.map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use rmcp::model::Content;
    use rmcp::transport::{
        StreamableHttpServerConfig,
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
    };

    #[derive(Clone)]
    struct TestMcpServer {
        tool_name: &'static str,
        response: &'static str,
    }

    impl ServerHandler for TestMcpServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo {
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
                tools: vec![Tool::new(
                    self.tool_name,
                    "Test tool",
                    serde_json::Map::new(),
                )],
                next_cursor: None,
                meta: None,
            })
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            Ok(CallToolResult::success(vec![Content::text(self.response)]))
        }
    }

    async fn start_http_server(server: TestMcpServer) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test MCP server");
        let address = listener.local_addr().expect("test MCP address");
        let mcp: StreamableHttpService<TestMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Default::default(),
                StreamableHttpServerConfig {
                    stateful_mode: false,
                    sse_keep_alive: None,
                    ..Default::default()
                },
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().nest_service("/mcp", mcp))
                .await
                .expect("serve test MCP");
        });
        (format!("http://{address}/mcp"), task)
    }

    fn http_server(name: &str, url: String) -> BridgeServerConfig {
        BridgeServerConfig {
            name: name.to_string(),
            transport: BridgeTransport::Http {
                url,
                headers: HashMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn bridge_aggregates_and_routes_http_tools() {
        let (first_url, first_task) = start_http_server(TestMcpServer {
            tool_name: "first_tool",
            response: "first response",
        })
        .await;
        let (second_url, second_task) = start_http_server(TestMcpServer {
            tool_name: "second_tool",
            response: "second response",
        })
        .await;
        let descriptor = BridgeDescriptor {
            servers: vec![
                http_server("first", first_url),
                http_server("second", second_url),
            ],
        };
        eprintln!("bridge test: connecting downstreams");
        let (bridge, mut downstreams) = build_bridge(Some(descriptor)).await;
        assert!(bridge.startup_error.is_none());
        eprintln!("bridge test: downstreams connected");

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (bridge_service, client) = tokio::join!(
            bridge.serve((server_read, server_write)),
            ().serve((client_read, client_write))
        );
        let mut bridge_service = bridge_service.expect("serve bridge");
        let mut client = client.expect("connect bridge");
        eprintln!("bridge test: client connected");

        let mut tools = client
            .peer()
            .list_all_tools()
            .await
            .expect("list bridge tools")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        tools.sort();
        assert_eq!(tools, vec!["first_tool", "second_tool"]);
        eprintln!("bridge test: tools listed");
        let result = client
            .peer()
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "second_tool".into(),
                arguments: None,
                task: None,
            })
            .await
            .expect("call routed tool");
        assert_eq!(
            result.content[0].as_text().expect("text result").text,
            "second response"
        );
        eprintln!("bridge test: tool routed");

        let _ = client.close_with_timeout(Duration::from_secs(1)).await;
        let _ = bridge_service
            .close_with_timeout(Duration::from_secs(1))
            .await;
        for downstream in &mut downstreams {
            let _ = downstream.close_with_timeout(Duration::from_secs(1)).await;
        }
        first_task.abort();
        second_task.abort();
        eprintln!("bridge test: cleanup complete");
    }

    #[tokio::test]
    async fn duplicate_downstream_tool_names_are_rejected() {
        let (first_url, first_task) = start_http_server(TestMcpServer {
            tool_name: "duplicate",
            response: "first",
        })
        .await;
        let (second_url, second_task) = start_http_server(TestMcpServer {
            tool_name: "duplicate",
            response: "second",
        })
        .await;
        let (bridge, mut downstreams) = build_bridge(Some(BridgeDescriptor {
            servers: vec![
                http_server("first", first_url),
                http_server("second", second_url),
            ],
        }))
        .await;
        assert_eq!(
            bridge.startup_error.as_deref(),
            Some("duplicate MCP tool name 'duplicate' across configured servers")
        );
        for downstream in &mut downstreams {
            let _ = downstream.close_with_timeout(Duration::from_secs(1)).await;
        }
        first_task.abort();
        second_task.abort();
    }

    #[tokio::test]
    async fn unavailable_downstream_does_not_hide_working_tools() {
        let (working_url, working_task) = start_http_server(TestMcpServer {
            tool_name: "working_tool",
            response: "working response",
        })
        .await;
        let (bridge, mut downstreams) = build_bridge(Some(BridgeDescriptor {
            servers: vec![
                http_server("working", working_url),
                http_server("unavailable", "http://127.0.0.1:1/mcp".to_string()),
            ],
        }))
        .await;
        assert!(bridge.startup_error.is_none());
        assert_eq!(bridge.tools.len(), 1);
        assert_eq!(bridge.tools[0].name, "working_tool");
        for downstream in &mut downstreams {
            let _ = downstream.close_with_timeout(Duration::from_secs(1)).await;
        }
        working_task.abort();
    }

    #[test]
    fn missing_descriptor_is_an_inert_bridge() {
        let bridge = empty_bridge(None);
        assert!(bridge.tools.is_empty());
        assert!(bridge.downstreams.is_empty());
        assert!(bridge.startup_error.is_none());
    }
}
