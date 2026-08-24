use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use serde_json::Value;

use crate::backend::TYDE_MCP_RESULT_ENVELOPE_KEY;

const DOWNSTREAM_STARTUP_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(250)
} else {
    Duration::from_secs(8)
};

pub const DESCRIPTOR_ENV: &str = "TYDE_HERMES_MCP_DESCRIPTOR";
pub const MANAGED_SERVER_NAME: &str = "tyde";
pub const DESCRIPTOR_FILE_NAME: &str = "tyde-mcp-servers.json";
pub const READY_FILE_NAME: &str = "tyde-mcp-ready.json";
static READY_PUBLISH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BridgeDescriptor {
    pub servers: Vec<BridgeServerConfig>,
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
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
                let sequence = READY_PUBLISH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let temporary_path = path.with_file_name(format!(
                    "{READY_FILE_NAME}.{}.{}.tmp",
                    std::process::id(),
                    sequence
                ));
                eprintln!(
                    "TYDE HERMES MCP READY PUBLISH sequence={sequence} path={}",
                    path.display()
                );
                let published = std::fs::write(&temporary_path, status.to_string())
                    .and_then(|_| std::fs::rename(&temporary_path, path.as_ref()));
                if let Err(error) = published {
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
        let mut result = self.downstreams[index]
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
            })?;
        let canonical = serde_json::to_value(&result).map_err(|error| {
            McpError::internal_error(
                format!("failed to preserve downstream MCP result: {error}"),
                None,
            )
        })?;
        let mut structured = match result.structured_content.take() {
            Some(Value::Object(object)) => object,
            Some(original) => {
                let mut object = serde_json::Map::new();
                object.insert("value".to_owned(), original);
                object
            }
            None => serde_json::Map::new(),
        };
        structured.insert(TYDE_MCP_RESULT_ENVELOPE_KEY.to_owned(), canonical);
        result.structured_content = Some(Value::Object(structured));
        result.is_error = Some(false);
        eprintln!(
            "TYDE HERMES MCP BRIDGE RESULT outbound={}",
            serde_json::to_value(&result).unwrap_or(Value::Null)
        );
        Ok(result)
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

    let startup_results =
        futures_util::future::join_all(descriptor.servers.into_iter().map(start_downstream)).await;
    for (server_name, result) in startup_results {
        let (client, server_tools) = match result {
            Ok(connected) => connected,
            Err(error) => {
                startup_errors.push(format!(
                    "failed to start configured MCP server '{server_name}': {error}"
                ));
                continue;
            }
        };
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
            name: server_name,
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

async fn start_downstream(
    server: BridgeServerConfig,
) -> (
    String,
    Result<(RunningService<RoleClient, ()>, Vec<Tool>), String>,
) {
    let name = server.name.clone();
    eprintln!("Tyde Hermes MCP bridge connecting configured server '{name}'");
    let result = tokio::time::timeout(DOWNSTREAM_STARTUP_TIMEOUT, async {
        let mut client = connect(&server).await.map_err(|error| error.to_string())?;
        eprintln!("Tyde Hermes MCP bridge connected configured server '{name}'");
        let tools = match client.peer().list_all_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                let _ = client.close_with_timeout(Duration::from_secs(1)).await;
                return Err(format!("failed to list tools: {error}"));
            }
        };
        eprintln!(
            "Tyde Hermes MCP bridge listed {} tools from configured server '{name}'",
            tools.len()
        );
        Ok((client, tools))
    })
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "timed out after {}ms",
            DOWNSTREAM_STARTUP_TIMEOUT.as_millis()
        ))
    });
    (name, result)
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
