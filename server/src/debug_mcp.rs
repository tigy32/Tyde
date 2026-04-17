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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

pub const DEBUG_REPO_ROOT_HEADER: &str = "x-tyde-debug-repo-root";
const START_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";

#[derive(Clone, Debug)]
pub struct DebugMcpHandle {
    pub url: String,
}

#[derive(Debug)]
struct DebugMcpState {
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

struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub fn start_server(bind_addr: Option<SocketAddr>) -> Result<DebugMcpHandle, String> {
    let bind_addr = bind_addr.unwrap_or_else(|| {
        DEFAULT_BIND_ADDR
            .parse()
            .expect("default loopback debug MCP bind addr must parse")
    });
    if !bind_addr.ip().is_loopback() {
        return Err(format!(
            "debug MCP server must bind to loopback only, got {bind_addr}"
        ));
    }

    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind debug MCP HTTP server on {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set debug MCP listener nonblocking: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read debug MCP listener addr: {err}"))?;
    let state = Arc::new(DebugMcpState {
        instances: Mutex::new(HashMap::new()),
    });
    std::thread::Builder::new()
        .name("tyde-debug-mcp".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build debug MCP runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener)
                    .expect("failed to create tokio debug MCP listener");
                run_accept_loop(listener, state).await;
            });
        })
        .map_err(|err| format!("failed to spawn debug MCP server thread: {err}"))?;

    Ok(DebugMcpHandle {
        url: format!("http://{local_addr}/mcp"),
    })
}

async fn run_accept_loop(listener: TcpListener, state: Arc<DebugMcpState>) {
    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(parts) => parts,
            Err(err) => {
                tracing::error!("debug MCP accept failed: {err}");
                continue;
            }
        };

        if !peer_addr.ip().is_loopback() {
            tracing::warn!("rejecting non-loopback debug MCP peer {peer_addr}");
            continue;
        }

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state).await {
                tracing::warn!("debug MCP HTTP connection failed: {err}");
            }
        });
    }
}

async fn handle_connection(mut stream: TcpStream, state: Arc<DebugMcpState>) -> Result<(), String> {
    let Some(request) = read_http_request(&mut stream).await? else {
        return Ok(());
    };

    let response = match request.target.as_str() {
        "/mcp" => handle_mcp_http_request(&state, request).await,
        _ => HttpResponse::text(404, "Not Found", "not found"),
    };

    write_http_response(&mut stream, response).await
}

async fn handle_mcp_http_request(state: &Arc<DebugMcpState>, request: HttpRequest) -> HttpResponse {
    if request.method != "POST" {
        return HttpResponse::text(405, "Method Not Allowed", "POST required");
    }

    let (target_path, query_repo_root) = split_request_target(&request.target);
    if target_path != "/mcp" {
        return HttpResponse::text(404, "Not Found", "not found");
    }

    let rpc_request: JsonRpcRequest = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(err) => {
            return HttpResponse::text(
                400,
                "Bad Request",
                &format!("invalid JSON-RPC request body: {err}"),
            );
        }
    };

    let repo_root = query_repo_root.or_else(|| {
        request
            .headers
            .get(DEBUG_REPO_ROOT_HEADER)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });

    match handle_rpc_request(state, repo_root.as_deref(), rpc_request).await {
        Ok(body) => HttpResponse::json(200, "OK", body),
        Err((status, reason, message)) => HttpResponse::text(status, reason, &message),
    }
}

async fn handle_rpc_request(
    state: &Arc<DebugMcpState>,
    repo_root: Option<&Path>,
    request: JsonRpcRequest,
) -> Result<Vec<u8>, (u16, &'static str, String)> {
    let response = match request.method.as_str() {
        "initialize" => {
            let Some(id) = request.id else {
                return Err((400, "Bad Request", "initialize requires an id".to_string()));
            };
            serde_json::to_vec(&JsonRpcResponse {
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
                    instructions: "Tyde server hosted debug MCP. Start a child Tyde dev instance with tyde_dev_instance_start, then inspect or drive its frontend with tyde_debug_evaluate. Dev instances are launched with hot reload disabled, so restart the instance when you want it to pick up code changes.".to_string(),
                },
            })
            .map_err(internal_json_error)?
        }
        "notifications/initialized" | "notifications/cancelled" => {
            serde_json::to_vec(&json!({})).map_err(internal_json_error)?
        }
        "ping" => {
            let Some(id) = request.id else {
                return Err((400, "Bad Request", "ping requires an id".to_string()));
            };
            serde_json::to_vec(&JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: json!({}),
            })
            .map_err(internal_json_error)?
        }
        "tools/list" => {
            let Some(id) = request.id else {
                return Err((400, "Bad Request", "tools/list requires an id".to_string()));
            };
            serde_json::to_vec(&JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: ToolsListResult {
                    tools: tool_definitions(),
                },
            })
            .map_err(internal_json_error)?
        }
        "tools/call" => {
            let Some(id) = request.id else {
                return Err((400, "Bad Request", "tools/call requires an id".to_string()));
            };
            let params: CallToolParams = serde_json::from_value(
                request.params.unwrap_or_else(|| json!({})),
            )
            .map_err(|err| {
                (
                    400,
                    "Bad Request",
                    format!("invalid tools/call params: {err}"),
                )
            })?;
            let result = dispatch_tool(state, repo_root, params).await;
            serde_json::to_vec(&JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result,
            })
            .map_err(internal_json_error)?
        }
        other => {
            let Some(id) = request.id else {
                return Err((400, "Bad Request", format!("method not found: {other}")));
            };
            serde_json::to_vec(&JsonRpcErrorResponse {
                jsonrpc: "2.0",
                id,
                error: JsonRpcErrorObject {
                    code: -32601,
                    message: format!("method not found: {other}"),
                },
            })
            .map_err(internal_json_error)?
        }
    };

    Ok(response)
}

fn internal_json_error(err: serde_json::Error) -> (u16, &'static str, String) {
    (
        500,
        "Internal Server Error",
        format!("failed to serialize debug MCP response JSON: {err}"),
    )
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "tyde_dev_instance_start",
            description: "Launch a Tyde desktop dev instance with hot reload disabled. Stop and restart it to pick up code changes. Waits until the typed host and UI-debug loopback endpoints are ready.",
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

async fn dispatch_tool(
    state: &Arc<DebugMcpState>,
    repo_root: Option<&Path>,
    params: CallToolParams,
) -> ToolCallResult {
    match params.name.as_str() {
        "tyde_dev_instance_start" => {
            let input = match parse_tool_input::<StartInstanceToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match start_instance(state, repo_root, input).await {
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
    state: &Arc<DebugMcpState>,
    repo_root: Option<&Path>,
    input: StartInstanceToolInput,
) -> Result<StartInstanceResult, String> {
    let project_dir = resolve_project_dir(repo_root, &input.project_dir)?;
    let instance_id = Uuid::new_v4().simple().to_string();
    let frontend_port = reserve_loopback_port()?;
    let host_port = reserve_loopback_port()?;
    let ui_debug_port = reserve_loopback_port()?;
    let host_addr = loopback_addr(host_port);
    let ui_debug_addr = loopback_addr(ui_debug_port);
    let frontend_url = format!("http://127.0.0.1:{frontend_port}");

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
    state: &Arc<DebugMcpState>,
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

async fn list_instances(state: &Arc<DebugMcpState>) -> Result<Vec<DevInstanceSummary>, String> {
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
    state: &Arc<DebugMcpState>,
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
                connect_host_endpoint(record.host_addr)
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

fn resolve_project_dir(repo_root: Option<&Path>, raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("project_dir must not be empty".to_string());
    }

    let path = PathBuf::from(trimmed);
    let joined = if path.is_absolute() {
        path
    } else {
        let repo_root = repo_root.ok_or_else(|| {
            format!(
                "relative project_dir requires repo_root in the MCP URL query or the {DEBUG_REPO_ROOT_HEADER} header"
            )
        })?;
        repo_root.join(path)
    };

    std::fs::canonicalize(&joined).map_err(|err| {
        format!(
            "failed to canonicalize project dir {}: {err}",
            joined.display()
        )
    })
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
        "trunk serve --port {frontend_port} --config frontend/Trunk.toml --no-autoreload"
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
    command.arg("--config").arg(config_path).arg("--no-watch");
    Ok(command)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

async fn read_http_request(stream: &mut TcpStream) -> Result<Option<HttpRequest>, String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let read = reader
        .read_line(&mut request_line)
        .await
        .map_err(|err| format!("failed to read debug MCP request line: {err}"))?;
    if read == 0 {
        return Ok(None);
    }

    let parts = request_line
        .trim_end_matches(['\r', '\n'])
        .split_whitespace()
        .collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!("invalid debug MCP request line: {request_line:?}"));
    }

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| format!("failed to read debug MCP header: {err}"))?;
        if read == 0 {
            return Err("unexpected EOF while reading debug MCP HTTP headers".to_string());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(format!("invalid debug MCP header line: {trimmed:?}"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let content_length = headers
        .get("content-length")
        .map(|raw| {
            raw.parse::<usize>()
                .map_err(|err| format!("invalid Content-Length header {raw:?}: {err}"))
        })
        .transpose()?
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|err| format!("failed to read debug MCP HTTP body: {err}"))?;

    Ok(Some(HttpRequest {
        method: parts[0].to_string(),
        target: parts[1].to_string(),
        headers,
        body,
    }))
}

fn split_request_target(target: &str) -> (&str, Option<PathBuf>) {
    let Some((path, query)) = target.split_once('?') else {
        return (target, None);
    };

    (path, parse_repo_root_from_query(query).map(PathBuf::from))
}

fn parse_repo_root_from_query(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "repo_root" {
            return percent_decode_query_component(value);
        }
    }
    None
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

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json",
            body,
        }
    }

    fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }
}

async fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<(), String> {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|err| format!("failed to write debug MCP HTTP header: {err}"))?;
    stream
        .write_all(&response.body)
        .await
        .map_err(|err| format!("failed to write debug MCP HTTP body: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("failed to flush debug MCP HTTP response: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_bind_addr() {
        let err = start_server(Some(
            "0.0.0.0:0"
                .parse()
                .expect("wildcard socket addr should parse"),
        ))
        .expect_err("non-loopback bind addr should be rejected");
        assert!(err.contains("loopback only"));
    }

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
            Value::String(
                "trunk serve --port 17777 --config frontend/Trunk.toml --no-autoreload".to_string()
            )
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tauri_dev_command_disables_watch() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        let config_path = repo_root.join("frontend/tauri-shell/tauri.conf.json");
        let command = tauri_dev_command(&repo_root, &config_path).expect("tauri dev command");
        let rendered = format!("{command:?}");
        assert!(
            rendered.contains("--no-watch"),
            "expected tauri dev command to disable watch, got {rendered}"
        );
    }

    #[test]
    fn split_request_target_reads_percent_encoded_repo_root() {
        let (path, repo_root) =
            split_request_target("/mcp?repo_root=%2FUsers%2Fmike%2FTyde%202&ignored=value");
        assert_eq!(path, "/mcp");
        assert_eq!(repo_root, Some(PathBuf::from("/Users/mike/Tyde 2")));
    }
}
