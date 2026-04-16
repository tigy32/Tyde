use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use client::ClientConfig;
use devtools_protocol::{UiDebugRequest, UiDebugResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const DEBUG_REPO_ROOT_ENV: &str = "TYDE_DEBUG_REPO_ROOT";
const START_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct DebugServerConfig {
    repo_root: PathBuf,
}

impl DebugServerConfig {
    pub fn from_args_env(args: &[String]) -> Result<Self, String> {
        match args {
            [flag, value] if flag == "--repo-root" => Ok(Self {
                repo_root: canonicalize_repo_root(PathBuf::from(value))?,
            }),
            [] => {
                if let Ok(value) = std::env::var(DEBUG_REPO_ROOT_ENV) {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        return Ok(Self {
                            repo_root: canonicalize_repo_root(PathBuf::from(trimmed))?,
                        });
                    }
                }
                Ok(Self {
                    repo_root: canonicalize_repo_root(
                        std::env::current_dir()
                            .map_err(|err| format!("failed to read current_dir: {err}"))?,
                    )?,
                })
            }
            _ => Err("usage: tyde-dev-driver debug [--repo-root /path/to/repo]".to_string()),
        }
    }
}

#[derive(Debug)]
struct DebugServerState {
    config: DebugServerConfig,
    instances: Mutex<HashMap<String, DevInstanceRecord>>,
}

#[derive(Debug)]
struct DevInstanceRecord {
    instance_id: String,
    project_dir: PathBuf,
    frontend_port: u16,
    host_addr: SocketAddr,
    ui_debug_addr: SocketAddr,
    frontend_url: String,
    config_path: PathBuf,
    child: Child,
    started_at_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevInstanceSummary {
    instance_id: String,
    project_dir: String,
    frontend_url: String,
    host_addr: String,
    ui_debug_addr: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse<T> {
    jsonrpc: &'static str,
    id: Value,
    result: T,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcErrorObject,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallToolParams {
    name: String,
    arguments: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    protocol_version: &'static str,
    capabilities: InitializeCapabilities,
    server_info: ServerInfoPayload,
    instructions: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilities {
    tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolsCapability {
    list_changed: bool,
}

#[derive(Debug, Serialize)]
struct ServerInfoPayload {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct ToolsListResult {
    tools: Vec<ToolDefinition>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResult {
    content: Vec<TextContent>,
    is_error: bool,
}

#[derive(Debug, Serialize)]
struct TextContent {
    #[serde(rename = "type")]
    type_name: &'static str,
    text: String,
}

impl ToolCallResult {
    fn json<T: Serialize>(value: T) -> Self {
        Self {
            content: vec![TextContent {
                type_name: "text",
                text: serde_json::to_string(&value)
                    .expect("tool result serialization should not fail"),
            }],
            is_error: false,
        }
    }

    fn text_error(message: impl Into<String>) -> Self {
        Self {
            content: vec![TextContent {
                type_name: "text",
                text: message.into(),
            }],
            is_error: true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StartInstanceToolInput {
    project_dir: String,
}

#[derive(Debug, Deserialize)]
struct StopInstanceToolInput {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct EvaluateToolInput {
    instance_id: String,
    expression: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartInstanceResult {
    instance_id: String,
    status: &'static str,
    project_dir: String,
    frontend_url: String,
    host_addr: String,
    ui_debug_addr: String,
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "tyde_dev_instance_start",
            description: "Launch a Tyde desktop dev instance on this host and wait until its typed host and UI-debug loopback endpoints are ready.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_dir": { "type": "string" }
                },
                "required": ["project_dir"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "tyde_dev_instance_stop",
            description: "Stop a previously launched Tyde dev instance.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "instance_id": { "type": "string" }
                },
                "required": ["instance_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "tyde_dev_instance_list",
            description: "List all Tyde dev instances currently launched by this MCP server.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "tyde_debug_evaluate",
            description: "Run JavaScript inside a launched Tyde dev instance frontend. The expression is used as the body of an async function, so use `return ...` when you want to return a value.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "instance_id": { "type": "string" },
                    "expression": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["instance_id", "expression"],
                "additionalProperties": false
            }),
        },
    ]
}

fn parse_tool_input<T: for<'de> Deserialize<'de>>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| format!("invalid tool arguments: {err}"))
}

async fn dispatch_tool(state: &Arc<DebugServerState>, params: CallToolParams) -> ToolCallResult {
    match params.name.as_str() {
        "tyde_dev_instance_start" => {
            let input = match parse_tool_input::<StartInstanceToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match start_instance(state, input).await {
                Ok(result) => ToolCallResult::json(result),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_dev_instance_stop" => {
            let input = match parse_tool_input::<StopInstanceToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match stop_instance(state, &input.instance_id).await {
                Ok(summary) => ToolCallResult::json(summary),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_dev_instance_list" => match list_instances(state).await {
            Ok(summaries) => ToolCallResult::json(summaries),
            Err(err) => ToolCallResult::text_error(err),
        },
        "tyde_debug_evaluate" => {
            let input = match parse_tool_input::<EvaluateToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            if input.expression.trim().is_empty() {
                return ToolCallResult::text_error("expression must not be empty");
            }
            match evaluate_instance(state, input).await {
                Ok(value) => ToolCallResult::json(json!({ "value": value })),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        other => ToolCallResult::text_error(format!("unknown tool '{other}'")),
    }
}

async fn start_instance(
    state: &Arc<DebugServerState>,
    input: StartInstanceToolInput,
) -> Result<StartInstanceResult, String> {
    let project_dir = resolve_project_dir(&state.config.repo_root, &input.project_dir)?;
    let frontend_port = reserve_loopback_port()?;
    let host_port = reserve_loopback_port()?;
    let ui_debug_port = reserve_loopback_port()?;
    let host_addr = loopback_addr(host_port);
    let ui_debug_addr = loopback_addr(ui_debug_port);
    let frontend_url = format!("http://127.0.0.1:{frontend_port}");
    let instance_id = Uuid::new_v4().simple().to_string();

    let config_path = write_dev_config(&project_dir, frontend_port, &instance_id)?;
    let mut command = tauri_dev_command(&project_dir, &config_path)?;
    command
        .current_dir(&project_dir)
        .env("TYDE_DEV_INSTANCE", "1")
        .env("TYDE_DEV_HOST_BIND_ADDR", host_addr.to_string())
        .env("TYDE_DEV_UI_DEBUG_BIND_ADDR", ui_debug_addr.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|err| format!("failed to spawn Tyde dev instance: {err}"))?;

    let mut record = DevInstanceRecord {
        instance_id: instance_id.clone(),
        project_dir,
        frontend_port,
        host_addr,
        ui_debug_addr,
        frontend_url: frontend_url.clone(),
        config_path,
        child,
        started_at_ms: now_ms(),
    };

    if let Err(err) = wait_for_instance_ready(&record).await {
        let _ = record.child.kill().await;
        let _ = tokio::fs::remove_file(&record.config_path).await;
        return Err(err);
    }

    let result = StartInstanceResult {
        instance_id: instance_id.clone(),
        status: "ready",
        project_dir: record.project_dir.display().to_string(),
        frontend_url: frontend_url.clone(),
        host_addr: record.host_addr.to_string(),
        ui_debug_addr: record.ui_debug_addr.to_string(),
    };

    let previous = state.instances.lock().await.insert(instance_id, record);
    assert!(previous.is_none(), "duplicate dev instance id inserted");

    Ok(result)
}

async fn stop_instance(
    state: &Arc<DebugServerState>,
    instance_id: &str,
) -> Result<DevInstanceSummary, String> {
    let mut record = state
        .instances
        .lock()
        .await
        .remove(instance_id)
        .ok_or_else(|| format!("unknown instance_id '{instance_id}'"))?;
    let _ = record.child.kill().await;
    let _ = tokio::fs::remove_file(&record.config_path).await;
    Ok(dev_instance_summary(&mut record).await)
}

async fn list_instances(state: &Arc<DebugServerState>) -> Result<Vec<DevInstanceSummary>, String> {
    let mut instances = state.instances.lock().await;
    let mut summaries = Vec::with_capacity(instances.len());
    let mut dead_ids = Vec::new();

    for (id, record) in instances.iter_mut() {
        let summary = dev_instance_summary(record).await;
        if summary.status != "running" {
            dead_ids.push(id.clone());
        }
        summaries.push(summary);
    }

    for id in dead_ids {
        if let Some(record) = instances.remove(&id) {
            let _ = tokio::fs::remove_file(record.config_path).await;
        }
    }

    summaries.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    Ok(summaries)
}

async fn evaluate_instance(
    state: &Arc<DebugServerState>,
    input: EvaluateToolInput,
) -> Result<Value, String> {
    let ui_debug_addr = {
        let instances = state.instances.lock().await;
        let record = instances
            .get(&input.instance_id)
            .ok_or_else(|| format!("unknown instance_id '{}'", input.instance_id))?;
        record.ui_debug_addr
    };

    let response = send_ui_debug_request(
        ui_debug_addr,
        UiDebugRequest::Evaluate {
            expression: input.expression,
            timeout_ms: input.timeout_ms,
        },
    )
    .await?;

    match response {
        UiDebugResponse::EvaluateResult { value } => Ok(value),
        UiDebugResponse::Error { message } => Err(message),
        other => Err(format!("unexpected evaluate response: {other:?}")),
    }
}

async fn wait_for_instance_ready(record: &DevInstanceRecord) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    loop {
        if started.elapsed() > START_TIMEOUT {
            return Err(format!(
                "timed out waiting for dev instance {} to become ready",
                record.instance_id
            ));
        }

        let host_ready = matches!(
            timeout(
                Duration::from_secs(2),
                connect_host_endpoint(record.host_addr),
            )
            .await,
            Ok(Ok(()))
        );

        let ui_ready = matches!(
            timeout(
                Duration::from_secs(2),
                send_ui_debug_request(record.ui_debug_addr, UiDebugRequest::Ping),
            )
            .await,
            Ok(Ok(UiDebugResponse::Pong))
        );

        if host_ready && ui_ready {
            return Ok(());
        }

        sleep(Duration::from_millis(250)).await;
    }
}

async fn dev_instance_summary(record: &mut DevInstanceRecord) -> DevInstanceSummary {
    let status = match record.child.try_wait() {
        Ok(Some(exit_status)) => format!("exited({exit_status})"),
        Ok(None) => "running".to_string(),
        Err(err) => format!("status_error({err})"),
    };
    let _ = record.frontend_port;
    let _ = record.started_at_ms;
    DevInstanceSummary {
        instance_id: record.instance_id.clone(),
        project_dir: record.project_dir.display().to_string(),
        frontend_url: record.frontend_url.clone(),
        host_addr: record.host_addr.to_string(),
        ui_debug_addr: record.ui_debug_addr.to_string(),
        status,
    }
}

async fn connect_host_endpoint(addr: SocketAddr) -> Result<(), String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|err| format!("failed to connect to host endpoint {addr}: {err}"))?;
    let _connection = client::connect(&ClientConfig::current(), stream)
        .await
        .map_err(|err| format!("host handshake failed for {addr}: {err:?}"))?;
    Ok(())
}

async fn send_ui_debug_request(
    addr: SocketAddr,
    request: UiDebugRequest,
) -> Result<UiDebugResponse, String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|err| format!("failed to connect to UI debug endpoint {addr}: {err}"))?;
    let body = serde_json::to_vec(&request)
        .map_err(|err| format!("failed to serialize UI debug request JSON: {err}"))?;
    stream
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write UI debug request: {err}"))?;
    stream
        .shutdown()
        .await
        .map_err(|err| format!("failed to flush UI debug request: {err}"))?;
    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .map_err(|err| format!("failed to read UI debug response: {err}"))?;
    serde_json::from_slice(&response_bytes)
        .map_err(|err| format!("failed to parse UI debug response JSON: {err}"))
}

fn canonicalize_repo_root(path: PathBuf) -> Result<PathBuf, String> {
    std::fs::canonicalize(&path)
        .map_err(|err| format!("failed to canonicalize repo root {}: {err}", path.display()))
}

fn resolve_project_dir(repo_root: &Path, raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("project_dir must not be empty".to_string());
    }
    let path = PathBuf::from(trimmed);
    let joined = if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    };
    canonicalize_repo_root(joined)
}

fn reserve_loopback_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("failed to reserve loopback port: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("failed to read reserved loopback port: {err}"))?
        .port();
    Ok(port)
}

fn loopback_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback socket addr must parse")
}

fn write_dev_config(
    repo_root: &Path,
    frontend_port: u16,
    instance_id: &str,
) -> Result<PathBuf, String> {
    let source_path = repo_root.join("frontend/tauri-shell/tauri.conf.json");
    let contents = std::fs::read_to_string(&source_path)
        .map_err(|err| format!("failed to read {}: {err}", source_path.display()))?;
    let mut json: Value = serde_json::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", source_path.display()))?;
    json["build"]["beforeDevCommand"] = Value::String(format!(
        "trunk serve --port {frontend_port} --config frontend/Trunk.toml"
    ));
    json["build"]["devUrl"] = Value::String(format!("http://127.0.0.1:{frontend_port}"));

    let output_path = std::env::temp_dir().join(format!("tyde-dev-instance-{instance_id}.json"));
    std::fs::write(
        &output_path,
        serde_json::to_vec_pretty(&json)
            .map_err(|err| format!("failed to serialize dev config override: {err}"))?,
    )
    .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;
    Ok(output_path)
}

fn tauri_dev_command(repo_root: &Path, config_path: &Path) -> Result<Command, String> {
    let local_cli = repo_root.join("node_modules/.bin/tauri");
    let mut command = if local_cli.is_file() {
        let mut command = Command::new(local_cli);
        command.arg("dev");
        command
    } else {
        let mut command = Command::new("npx");
        command.arg("tauri").arg("dev");
        command
    };
    command.arg("--config").arg(config_path);
    Ok(command)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

async fn handle_request<W: AsyncWrite + Unpin>(
    state: &Arc<DebugServerState>,
    writer: &mut W,
    request: JsonRpcRequest,
) -> Result<(), String> {
    match request.method.as_str() {
        "initialize" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: InitializeResult {
                        protocol_version: "2025-03-26",
                        capabilities: InitializeCapabilities {
                            tools: ToolsCapability { list_changed: false },
                        },
                        server_info: ServerInfoPayload {
                            name: "tyde-debug",
                            version: "0.0.0",
                        },
                        instructions: "Backend-owned Tyde debug tools. Start a child Tyde dev instance on this host with tyde_dev_instance_start, then inspect or drive its frontend with tyde_debug_evaluate.".to_string(),
                    },
                },
            )
            .await
        }
        "notifications/initialized" => Ok(()),
        "ping" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: json!({}),
                },
            )
            .await
        }
        "tools/list" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: ToolsListResult {
                        tools: tool_definitions(),
                    },
                },
            )
            .await
        }
        "tools/call" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            let params: CallToolParams =
                serde_json::from_value(request.params.unwrap_or_else(|| json!({})))
                    .map_err(|err| format!("invalid tools/call params: {err}"))?;
            let result = dispatch_tool(state, params).await;
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result,
                },
            )
            .await
        }
        "notifications/cancelled" => Ok(()),
        other => {
            if let Some(id) = request.id {
                write_mcp_message(
                    writer,
                    &JsonRpcErrorResponse {
                        jsonrpc: "2.0",
                        id,
                        error: JsonRpcErrorObject {
                            code: -32601,
                            message: format!("method not found: {other}"),
                        },
                    },
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn read_mcp_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| format!("failed to read MCP header: {err}"))?;

        if read == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            return Err("unexpected EOF while reading MCP headers".to_string());
        }

        if line == "\r\n" || line == "\n" {
            break;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|err| format!("invalid Content-Length header '{trimmed}': {err}"))?;
            content_length = Some(parsed);
        }
    }

    let Some(content_length) = content_length else {
        return Err("missing Content-Length header".to_string());
    };
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|err| format!("failed to read MCP body: {err}"))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| format!("failed to parse MCP JSON body: {err}"))
}

async fn write_mcp_message<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), String> {
    let body =
        serde_json::to_vec(value).map_err(|err| format!("failed to serialize MCP JSON: {err}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|err| format!("failed to write MCP header: {err}"))?;
    writer
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write MCP body: {err}"))?;
    writer
        .flush()
        .await
        .map_err(|err| format!("failed to flush MCP output: {err}"))?;
    Ok(())
}

pub async fn run_stdio_server(config: DebugServerConfig) -> Result<(), String> {
    let state = Arc::new(DebugServerState {
        config,
        instances: Mutex::new(HashMap::new()),
    });
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = stdout;

    loop {
        let Some(message) = read_mcp_message(&mut reader).await? else {
            return Ok(());
        };
        let request: JsonRpcRequest = serde_json::from_value(message)
            .map_err(|err| format!("invalid JSON-RPC request: {err}"))?;
        handle_request(&state, &mut writer, request).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_dev_config_overrides_frontend_port() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        let path = write_dev_config(&repo_root, 17777, "test-instance").expect("write config");
        let contents = std::fs::read_to_string(&path).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("parse config");
        assert_eq!(
            json["build"]["devUrl"],
            Value::String("http://127.0.0.1:17777".to_string())
        );
        assert_eq!(
            json["build"]["beforeDevCommand"],
            Value::String("trunk serve --port 17777 --config frontend/Trunk.toml".to_string())
        );
        let _ = std::fs::remove_file(path);
    }
}
