use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use client::ClientConfig;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use devtools_protocol::{
    BoundedDebugOutput, DebugOutputSlice, UiDebugRequest, UiDebugResponse,
    dev_instance_mutable_paths,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use protocol::{Project, ProjectId, ProjectRootPath, ProjectSource};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const DEBUG_REPO_ROOT_ENV: &str = "TYDE_DEBUG_REPO_ROOT";
const START_TIMEOUT: Duration = Duration::from_secs(105);
const STARTUP_LOG_TAIL_BYTES: usize = 32 * 1024;

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
    store_dir: PathBuf,
    startup_output: Arc<StdMutex<BoundedDebugOutput>>,
    startup_capture_tasks: Vec<tokio::task::JoinHandle<()>>,
    child: AsyncGroupChild,
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
    frontend_port: u16,
    started_at_ms: u64,
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

fn debug_capabilities() -> DebugCapabilities {
    DebugCapabilities {
        process_output_events: true,
        monotonic_output_cursors: true,
        instance_snapshot: true,
        ui_evaluate: true,
        screenshot: false,
        second_client: false,
        screenshot_reason: "the desktop UI-debug endpoint does not implement capture_screenshot",
        second_client_reason: "the debug launcher does not expose an isolated second-client harness",
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInstanceToolInput {
    project_dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInstanceToolInput {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateToolInput {
    instance_id: String,
    expression: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DebugEventsToolInput {
    instance_id: String,
    cursor: Option<u64>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DebugSnapshotToolInput {
    instance_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DebugCapabilities {
    process_output_events: bool,
    monotonic_output_cursors: bool,
    instance_snapshot: bool,
    ui_evaluate: bool,
    screenshot: bool,
    second_client: bool,
    screenshot_reason: &'static str,
    second_client_reason: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DebugEventsResult {
    instance_id: String,
    events: Vec<DebugOutputEvent>,
    next_cursor: u64,
    oldest_cursor: u64,
    truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DebugOutputEvent {
    cursor: u64,
    kind: &'static str,
    output: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DebugSnapshotResult {
    instance: DevInstanceSummary,
    ready: bool,
    output_cursor: u64,
    capabilities: DebugCapabilities,
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
        ToolDefinition {
            name: "tyde_debug_events",
            description: "Read bounded combined process output from a launched instance. Pass the returned nextCursor to resume without rereading output; truncated is true if the requested cursor fell behind the retained window.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "instance_id": { "type": "string" },
                    "cursor": { "type": "integer", "minimum": 0 },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": STARTUP_LOG_TAIL_BYTES }
                },
                "required": ["instance_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "tyde_debug_snapshot",
            description: "Return a non-visual snapshot of a launched instance's process status, readiness, output cursor, and supported debug capabilities.",
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
            name: "tyde_debug_capabilities",
            description: "Report debug capabilities explicitly. Unsupported screenshot and second-client automation are false so QA can fail closed.",
            input_schema: json!({
                "type": "object",
                "properties": {},
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
        "tyde_debug_events" => {
            let input = match parse_tool_input::<DebugEventsToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match debug_events(state, input).await {
                Ok(result) => ToolCallResult::json(result),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_debug_snapshot" => {
            let input = match parse_tool_input::<DebugSnapshotToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match debug_snapshot(state, &input.instance_id).await {
                Ok(result) => ToolCallResult::json(result),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_debug_capabilities" => {
            if let Err(err) = parse_tool_input::<EmptyToolInput>(params.arguments) {
                return ToolCallResult::text_error(err);
            }
            ToolCallResult::json(debug_capabilities())
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
    let store_dir = dev_instance_store_dir(&instance_id);
    std::fs::create_dir_all(&store_dir).map_err(|err| {
        format!(
            "failed to create dev instance store dir {}: {err}",
            store_dir.display()
        )
    })?;
    seed_dev_project_store(&store_dir, &project_dir, &instance_id)?;

    let config_path = write_dev_config(&project_dir, frontend_port, &instance_id)?;
    let mut command = tauri_dev_command(&config_path)?;
    command
        .current_dir(&project_dir)
        .env("TYDE_DEV_INSTANCE", "1")
        .env("TYDE_DEV_HOST_BIND_ADDR", host_addr.to_string())
        .env("TYDE_DEV_UI_DEBUG_BIND_ADDR", ui_debug_addr.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_dev_instance_environment(&mut command, &store_dir);

    let mut child = command
        .group_spawn()
        .map_err(|err| format!("failed to spawn Tyde dev instance: {err}"))?;
    let startup_output = Arc::new(StdMutex::new(BoundedDebugOutput::new(
        STARTUP_LOG_TAIL_BYTES,
    )));
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| "failed to capture Tyde dev instance stdout".to_string())?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| "failed to capture Tyde dev instance stderr".to_string())?;
    let startup_capture_tasks = vec![
        tokio::spawn(capture_startup_output(stdout, Arc::clone(&startup_output))),
        tokio::spawn(capture_startup_output(stderr, Arc::clone(&startup_output))),
    ];

    let mut record = DevInstanceRecord {
        instance_id: instance_id.clone(),
        project_dir,
        frontend_port,
        host_addr,
        ui_debug_addr,
        frontend_url: frontend_url.clone(),
        config_path,
        store_dir,
        startup_output,
        startup_capture_tasks,
        child,
        started_at_ms: now_ms(),
    };

    if let Err(err) = wait_for_instance_ready(&mut record).await {
        let _ = record.child.kill().await;
        let _ = tokio::fs::remove_file(&record.config_path).await;
        let _ = tokio::fs::remove_dir_all(&record.store_dir).await;
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

fn configure_dev_instance_environment(command: &mut Command, store_dir: &Path) {
    for (env, path) in dev_instance_mutable_paths(store_dir) {
        command.env(env, path);
    }
}

fn seed_dev_project_store(
    store_dir: &Path,
    project_dir: &Path,
    instance_id: &str,
) -> Result<(), String> {
    let project_id = ProjectId(format!("dev-{instance_id}"));
    let name = project_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Tyde Dev Project")
        .to_owned();
    let project = Project {
        id: project_id.clone(),
        name,
        sort_order: 0,
        source: ProjectSource::Standalone {
            roots: vec![ProjectRootPath(project_dir.display().to_string())],
        },
    };
    let records = HashMap::from([(project_id.0.clone(), project)]);
    let contents = json!({ "version": 2, "records": records });
    std::fs::write(
        store_dir.join("projects.json"),
        serde_json::to_vec_pretty(&contents)
            .map_err(|err| format!("failed to serialize dev project store: {err}"))?,
    )
    .map_err(|err| format!("failed to seed dev project store: {err}"))
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
    let _ = tokio::fs::remove_dir_all(&record.store_dir).await;
    Ok(dev_instance_summary(&mut record).await)
}

async fn list_instances(state: &Arc<DebugServerState>) -> Result<Vec<DevInstanceSummary>, String> {
    let mut instances = state.instances.lock().await;
    let mut summaries = Vec::with_capacity(instances.len());

    for record in instances.values_mut() {
        let summary = dev_instance_summary(record).await;
        summaries.push(summary);
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

async fn debug_events(
    state: &Arc<DebugServerState>,
    input: DebugEventsToolInput,
) -> Result<DebugEventsResult, String> {
    let max_bytes = input.max_bytes.unwrap_or(STARTUP_LOG_TAIL_BYTES);
    if !(1..=STARTUP_LOG_TAIL_BYTES).contains(&max_bytes) {
        return Err(format!(
            "max_bytes must be between 1 and {STARTUP_LOG_TAIL_BYTES}"
        ));
    }
    let output = {
        let instances = state.instances.lock().await;
        let record = instances
            .get(&input.instance_id)
            .ok_or_else(|| format!("unknown instance_id '{}'", input.instance_id))?;
        let output = record
            .startup_output
            .lock()
            .expect("startup output mutex poisoned")
            .read(input.cursor, max_bytes);
        output
    };
    Ok(debug_events_result(input.instance_id, output))
}

fn debug_events_result(instance_id: String, output: DebugOutputSlice) -> DebugEventsResult {
    let events = (!output.output.is_empty())
        .then(|| DebugOutputEvent {
            cursor: output.cursor,
            kind: "process_output",
            output: output.output,
        })
        .into_iter()
        .collect();
    DebugEventsResult {
        instance_id,
        events,
        next_cursor: output.next_cursor,
        oldest_cursor: output.oldest_cursor,
        truncated: output.truncated,
    }
}

async fn debug_snapshot(
    state: &Arc<DebugServerState>,
    instance_id: &str,
) -> Result<DebugSnapshotResult, String> {
    let mut instances = state.instances.lock().await;
    let record = instances
        .get_mut(instance_id)
        .ok_or_else(|| format!("unknown instance_id '{instance_id}'"))?;
    let output_cursor = record
        .startup_output
        .lock()
        .expect("startup output mutex poisoned")
        .next_cursor();
    let instance = dev_instance_summary(record).await;
    let ready = instance.status == "running";
    Ok(DebugSnapshotResult {
        instance,
        ready,
        output_cursor,
        capabilities: debug_capabilities(),
    })
}

async fn wait_for_instance_ready(record: &mut DevInstanceRecord) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    loop {
        match record.child.try_wait() {
            Ok(Some(exit_status)) => {
                for mut task in record.startup_capture_tasks.drain(..) {
                    if timeout(Duration::from_millis(250), &mut task)
                        .await
                        .is_err()
                    {
                        task.abort();
                    }
                }
                return Err(with_startup_diagnostics(
                    format!(
                        "dev instance {} exited before ready: {exit_status}",
                        record.instance_id
                    ),
                    &record.startup_output,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                return Err(with_startup_diagnostics(
                    format!(
                        "failed to read dev instance {} process status: {err}",
                        record.instance_id
                    ),
                    &record.startup_output,
                ));
            }
        }

        if started.elapsed() > START_TIMEOUT {
            return Err(with_startup_diagnostics(
                format!(
                    "timed out waiting for dev instance {} to become ready",
                    record.instance_id
                ),
                &record.startup_output,
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
    DevInstanceSummary {
        instance_id: record.instance_id.clone(),
        project_dir: record.project_dir.display().to_string(),
        frontend_url: record.frontend_url.clone(),
        host_addr: record.host_addr.to_string(),
        ui_debug_addr: record.ui_debug_addr.to_string(),
        status,
        frontend_port: record.frontend_port,
        started_at_ms: record.started_at_ms,
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

fn dev_instance_store_dir(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tyde-dev-instance-{instance_id}"))
}

fn write_dev_config(
    repo_root: &Path,
    frontend_port: u16,
    instance_id: &str,
) -> Result<PathBuf, String> {
    let source_path = repo_root.join("frontend/tauri-shell/tauri.conf.json");
    let trunk_command_path = repo_root.join("tools/trunk-command.mjs");
    let contents = std::fs::read_to_string(&source_path)
        .map_err(|err| format!("failed to read {}: {err}", source_path.display()))?;
    let mut json: Value = serde_json::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", source_path.display()))?;
    json["build"]["beforeDevCommand"] = Value::String(format!(
        "node {} serve --port {frontend_port} --no-autoreload",
        shell_single_quote(&trunk_command_path.display().to_string())
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

fn tauri_dev_command(config_path: &Path) -> Result<Command, String> {
    let cargo_tauri = resolve_cargo_tauri().ok_or_else(|| {
        "cargo-tauri was not found in PATH or the Cargo bin directory; install the Tauri CLI and ensure cargo-tauri is available before starting a Tyde dev instance (the launcher does not use npx or install packages)".to_string()
    })?;
    Ok(tauri_dev_command_with_cli(config_path, &cargo_tauri))
}

fn tauri_dev_command_with_cli(config_path: &Path, cargo_tauri: &Path) -> Command {
    let mut command = Command::new(cargo_tauri);
    command.arg("dev");
    command.arg("--config").arg(config_path).arg("--no-watch");
    command
}

fn resolve_cargo_tauri() -> Option<PathBuf> {
    let mut dirs = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        dirs.push(PathBuf::from(cargo_home).join("bin"));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    find_cargo_tauri_in_dirs(dirs)
}

fn find_cargo_tauri_in_dirs(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join(format!("cargo-tauri{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn capture_startup_output(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    output: Arc<StdMutex<BoundedDebugOutput>>,
) {
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => return,
            Ok(count) => append_startup_output(&output, &chunk[..count]),
            Err(_) => return,
        }
    }
}

fn append_startup_output(output: &StdMutex<BoundedDebugOutput>, bytes: &[u8]) {
    let mut output = output.lock().expect("startup output mutex poisoned");
    output.append(bytes);
}

fn with_startup_diagnostics(
    message: String,
    output: &StdMutex<BoundedDebugOutput>,
) -> String {
    let output = output.lock().expect("startup output mutex poisoned");
    let diagnostics = if output.is_empty() {
        "startup output was empty".to_string()
    } else {
        let tail = output.read(None, STARTUP_LOG_TAIL_BYTES);
        format!(
            "startup output (last {} bytes):\n{}",
            output.len(),
            tail.output.trim_end()
        )
    };
    format!("{message}\n{diagnostics}")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
                        instructions: "Backend-owned Tyde debug tools. Start a child Tyde dev instance with tyde_dev_instance_start; inspect bounded process output with tyde_debug_events, take a non-visual status snapshot with tyde_debug_snapshot, and drive its frontend with tyde_debug_evaluate. Check tyde_debug_capabilities before requiring screenshots or a second client; unsupported capabilities are reported false.".to_string(),
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
    fn evaluate_tool_input_rejects_unknown_fields() {
        let mut args = Map::new();
        args.insert("instance_id".to_string(), json!("instance"));
        args.insert("expression".to_string(), json!("return 1"));
        args.insert("timeoutMs".to_string(), json!(1000));

        let err = parse_tool_input::<EvaluateToolInput>(Some(args))
            .expect_err("unknown tool argument should be rejected");
        assert!(
            err.contains("unknown field") && err.contains("timeoutMs"),
            "unexpected error: {err}"
        );
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
            Value::String(format!(
                "node '{}' serve --port 17777 --no-autoreload",
                repo_root.join("tools/trunk-command.mjs").display()
            ))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tauri_dev_command_uses_resolved_cargo_tauri_without_install_fallback() {
        let config_path = Path::new("/repo/frontend/tauri-shell/tauri.conf.json");
        let command =
            tauri_dev_command_with_cli(config_path, Path::new("/toolchain/bin/cargo-tauri"));
        let rendered = format!("{command:?}");

        assert!(rendered.contains("/toolchain/bin/cargo-tauri"));
        assert!(rendered.contains("dev"));
        assert!(rendered.contains("--config"));
        assert!(rendered.contains("--no-watch"));
        assert!(!rendered.contains("npx"));
        assert!(!rendered.contains("node_modules"));
    }

    #[test]
    fn cargo_tauri_resolution_uses_explicit_search_order() {
        let first = tempfile::tempdir().expect("first temp dir");
        let second = tempfile::tempdir().expect("second temp dir");
        let expected = second
            .path()
            .join(format!("cargo-tauri{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&expected, b"test executable").expect("write candidate");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = expected
                .metadata()
                .expect("candidate metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&expected, permissions).expect("make candidate executable");
        }

        let resolved = find_cargo_tauri_in_dirs(vec![
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);

        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn startup_diagnostics_include_bounded_output_tail() {
        let marker = b"actionable cargo-tauri failure\n";
        let output = StdMutex::new(BoundedDebugOutput::new(STARTUP_LOG_TAIL_BYTES));
        append_startup_output(&output, &vec![b'x'; STARTUP_LOG_TAIL_BYTES + 17]);
        append_startup_output(&output, marker);

        let message = with_startup_diagnostics("exited before ready".to_string(), &output);

        assert!(message.contains("exited before ready"));
        assert!(message.contains("actionable cargo-tauri failure"));
        assert_eq!(
            output.lock().expect("startup output").len(),
            STARTUP_LOG_TAIL_BYTES
        );
    }

    #[test]
    fn debug_capabilities_fail_closed_for_unsupported_surfaces() {
        let capabilities = debug_capabilities();
        assert!(!capabilities.screenshot);
        assert!(!capabilities.second_client);
        assert!(capabilities.process_output_events);
        assert!(capabilities.monotonic_output_cursors);
    }

    #[test]
    fn debug_events_exposes_resume_cursor_without_empty_event() {
        let result = debug_events_result(
            "instance".to_owned(),
            DebugOutputSlice {
                cursor: 12,
                next_cursor: 12,
                oldest_cursor: 4,
                truncated: false,
                output: String::new(),
            },
        );
        assert!(result.events.is_empty());
        assert_eq!(result.next_cursor, 12);
        assert_eq!(result.oldest_cursor, 4);
    }

    #[test]
    fn dev_instance_environment_isolates_every_mutable_path() {
        let store_dir = Path::new("/isolated/tyde-instance");
        let mut command = Command::new("tyde");
        configure_dev_instance_environment(&mut command, store_dir);
        let configured = command.as_std().get_envs().collect::<HashMap<_, _>>();

        for entry in devtools_protocol::DEV_INSTANCE_MUTABLE_PATHS {
            let expected_path = store_dir.join(entry.relative_path);
            assert_eq!(
                configured.get(std::ffi::OsStr::new(entry.env)).copied().flatten(),
                Some(expected_path.as_os_str()),
                "launcher did not isolate {}",
                entry.env
            );
        }
    }

    #[test]
    fn dev_instance_seeds_only_requested_project() {
        let store = tempfile::tempdir().expect("store dir");
        let project = tempfile::tempdir().expect("project dir");
        seed_dev_project_store(store.path(), project.path(), "instance")
            .expect("seed project store");
        let contents: Value = serde_json::from_slice(
            &std::fs::read(store.path().join("projects.json")).expect("read project store"),
        )
        .expect("parse project store");
        let records = contents["records"].as_object().expect("records object");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records["dev-instance"]["source"]["Standalone"]["roots"][0],
            Value::String(project.path().display().to_string())
        );
    }
}
