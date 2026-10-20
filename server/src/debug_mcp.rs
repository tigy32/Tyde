use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::process_env::launch::{
    AsyncCommandGroup, AsyncGroupChild, GroupCommand as Command, ProcessStdio as Stdio,
};
use axum::{Json, Router, response::IntoResponse, routing::get};
use client::ClientConfig;
use devtools_protocol::{
    BoundedDebugOutput, CLAUDE_SECURESTORAGE_CONFIG_DIR_ENV, DEV_INSTANCE_DENY_PROXY_URL,
    DEV_INSTANCE_HERMES_EXECUTABLE_ENV, DEV_INSTANCE_HERMES_HOME_ENV,
    DEV_INSTANCE_HERMES_PYTHON_ENV, DEV_INSTANCE_HOME_ENV, DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS,
    DebugOutputSlice, DevInstanceHermesEnvironmentAttestation, DevInstanceStartupCleanup,
    DisposableHermesEnvironment, PreparedDisposableHermesEnvironment, UiDebugRequest,
    UiDebugResponse, dev_instance_mutable_paths, is_provider_environment_key,
    parent_claude_credential_store_dir, prepare_disposable_hermes_environment,
    resolve_parent_hermes_runtime,
};
use protocol::{Project, ProjectId, ProjectRootPath, ProjectSource};
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
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::process_env;

pub const DEBUG_REPO_ROOT_HEADER: &str = "x-tyde-debug-repo-root";
const STARTUP_LOG_TAIL_BYTES: usize = 32 * 1024;
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";
#[derive(Clone)]
pub struct DebugMcpHandle {
    pub url: String,
    project_host: Arc<std::sync::OnceLock<crate::host::WeakHostHandle>>,
    instances: Option<mpsc::UnboundedSender<InstanceCommand>>,
}

impl std::fmt::Debug for DebugMcpHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugMcpHandle")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl DebugMcpHandle {
    pub(crate) fn unavailable() -> Self {
        Self {
            url: String::new(),
            project_host: Arc::new(std::sync::OnceLock::new()),
            instances: None,
        }
    }

    pub(crate) fn bind_host(&self, host: &crate::host::HostHandle) -> Result<(), String> {
        self.project_host
            .set(crate::host::WeakHostHandle::downgrade(host))
            .map_err(|_| "Debug MCP host ownership is already bound".to_owned())
    }

    pub(crate) async fn stop_workbench(&self, project_id: &ProjectId) -> Result<(), String> {
        let Some(instances) = &self.instances else {
            return Ok(());
        };
        let (reply, result) = oneshot::channel();
        instances
            .send(InstanceCommand::CloseProject {
                project_id: project_id.clone(),
                reply,
            })
            .map_err(|_| "Dev instance actor stopped".to_owned())?;
        result
            .await
            .map_err(|_| "Dev instance cleanup did not complete".to_owned())?
    }
}

struct DebugMcpState {
    instances: mpsc::UnboundedSender<InstanceCommand>,
    project_host: Arc<std::sync::OnceLock<crate::host::WeakHostHandle>>,
}

enum InstanceCommand {
    Start {
        project_dir: PathBuf,
        input: StartInstanceToolInput,
        execution_project: Option<Box<Project>>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<StartInstanceResult, String>>,
    },
    CloseProject {
        project_id: ProjectId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Remove {
        id: String,
        reply: oneshot::Sender<Result<DevInstanceSummary, String>>,
    },
    List {
        reply: oneshot::Sender<Vec<DevInstanceSummary>>,
    },
    Address {
        id: String,
        reply: oneshot::Sender<Result<SocketAddr, String>>,
    },
    Output {
        id: String,
        cursor: Option<u64>,
        max_bytes: usize,
        reply: oneshot::Sender<Result<DebugOutputSlice, String>>,
    },
    Snapshot {
        id: String,
        reply: oneshot::Sender<Result<DebugSnapshotResult, String>>,
    },
}

struct PendingDevStart {
    project_id: Option<ProjectId>,
    cancellation: CancellationToken,
    reply: oneshot::Sender<Result<StartInstanceResult, String>>,
}

struct DevStartFailure {
    message: String,
    cleanup_error: Option<String>,
}

impl From<String> for DevStartFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            cleanup_error: None,
        }
    }
}

impl DevStartFailure {
    fn with_cleanup(mut self, error: String) -> Self {
        self.message = format!("{}; {error}", self.message);
        self.cleanup_error = Some(match self.cleanup_error {
            Some(previous) => format!("{previous}; {error}"),
            None => error,
        });
        self
    }
}

#[derive(Default)]
struct ClosingDevProject {
    replies: Vec<oneshot::Sender<Result<(), String>>>,
    error: Option<String>,
}

fn start_instance_registry(mut receiver: mpsc::UnboundedReceiver<InstanceCommand>) {
    tokio::spawn(async move {
        let mut records = HashMap::<String, DevInstanceRecord>::new();
        let mut closed_projects = std::collections::HashSet::new();
        let mut closing = HashMap::<ProjectId, ClosingDevProject>::new();
        let mut starts = tokio::task::JoinSet::new();
        let mut pending = HashMap::<tokio::task::Id, PendingDevStart>::new();
        let mut receiving = true;
        loop {
            let command = tokio::select! {
                command = receiver.recv(), if receiving => match command {
                    Some(command) => Some(command),
                    None => {
                        receiving = false;
                        for start in pending.values() {
                            start.cancellation.cancel();
                        }
                        None
                    },
                },
                completed = starts.join_next_with_id(), if !starts.is_empty() => {
                    if let Some(completed) = completed {
                        let (id, result) = match completed {
                            Ok(value) => value,
                            Err(error) => (error.id(), Err(DevStartFailure::from(
                                "DEV startup task failed".to_owned(),
                            ).with_cleanup("DEV startup cleanup could not be confirmed".to_owned()))),
                        };
                        if let Some(start) = pending.remove(&id) {
                            let result = match result {
                                Ok((record, result)) => {
                                    if start.reply.is_closed() || start.project_id.as_ref().is_some_and(|project| closed_projects.contains(project)) {
                                        match stop_record(record).await {
                                            Ok(_) => Err("DEV startup ownership was withdrawn; its processes were stopped".to_owned()),
                                            Err(error) => {
                                                if let Some(project) = &start.project_id
                                                    && let Some(state) = closing.get_mut(project) {
                                                        state.error = Some(error.clone());
                                                    }
                                                Err(error)
                                            }
                                        }
                                    } else if records.contains_key(&record.instance_id) {
                                        match stop_record(record).await {
                                            Ok(_) => Err("Duplicate DEV instance identity".to_owned()),
                                            Err(error) => Err(error),
                                        }
                                    } else {
                                        records.insert(record.instance_id.clone(), record);
                                        Ok(result)
                                    }
                                },
                                Err(error) => {
                                    if let Some(project) = &start.project_id
                                        && let Some(state) = closing.get_mut(project)
                                        && let Some(cleanup) = error.cleanup_error {
                                            state.error = Some(cleanup);
                                        }
                                    Err(error.message)
                                },
                            };
                            let _ = start.reply.send(result);
                        }
                    }
                    None
                },
                else => break,
            };
            if let Some(command) = command {
                match command {
                    InstanceCommand::Start {
                        project_dir,
                        input,
                        execution_project,
                        cancellation,
                        reply,
                    } => {
                        let project_id =
                            execution_project.as_ref().map(|project| project.id.clone());
                        if cancellation.is_cancelled() {
                            let _ =
                                reply.send(Err("DEV startup cancelled before launch".to_owned()));
                        } else if project_id
                            .as_ref()
                            .is_some_and(|project| closed_projects.contains(project))
                        {
                            let _ = reply.send(Err(
                                "Workbench is being removed; no DEV process was launched"
                                    .to_owned(),
                            ));
                        } else {
                            let task = starts.spawn(build_instance(
                                project_dir,
                                input,
                                execution_project.map(|project| *project),
                                cancellation.clone(),
                            ));
                            pending.insert(
                                task.id(),
                                PendingDevStart {
                                    project_id,
                                    cancellation,
                                    reply,
                                },
                            );
                        }
                    }
                    InstanceCommand::CloseProject { project_id, reply } => {
                        closed_projects.insert(project_id.clone());
                        for start in pending.values() {
                            if start.project_id.as_ref() == Some(&project_id) {
                                start.cancellation.cancel();
                            }
                        }
                        closing
                            .entry(project_id.clone())
                            .or_default()
                            .replies
                            .push(reply);
                        let ids = records
                            .iter()
                            .filter(|(_, record)| record.project_id.as_ref() == Some(&project_id))
                            .map(|(id, _)| id.clone())
                            .collect::<Vec<_>>();
                        for id in ids {
                            if let Some(record) = records.remove(&id)
                                && let Err(error) = stop_record(record).await
                            {
                                closing.entry(project_id.clone()).or_default().error = Some(error);
                            }
                        }
                    }
                    InstanceCommand::Remove { id, reply } => {
                        let result = match records.remove(&id) {
                            Some(record) => stop_record(record).await,
                            None => Err("Unknown DEV instance".to_owned()),
                        };
                        let _ = reply.send(result);
                    }
                    InstanceCommand::List { reply } => {
                        let mut summaries = records
                            .values_mut()
                            .map(dev_instance_summary)
                            .collect::<Vec<_>>();
                        summaries.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
                        let _ = reply.send(summaries);
                    }
                    InstanceCommand::Address { id, reply } => {
                        let _ = reply.send(
                            records
                                .get(&id)
                                .map(|record| record.ui_debug_addr)
                                .ok_or_else(|| format!("unknown instance_id '{id}'")),
                        );
                    }
                    InstanceCommand::Output {
                        id,
                        cursor,
                        max_bytes,
                        reply,
                    } => {
                        let _ = reply.send(
                            records
                                .get(&id)
                                .map(|record| {
                                    record
                                        .startup_output
                                        .lock()
                                        .expect("startup output mutex poisoned")
                                        .read(cursor, max_bytes)
                                })
                                .ok_or_else(|| format!("unknown instance_id '{id}'")),
                        );
                    }
                    InstanceCommand::Snapshot { id, reply } => {
                        let result = records
                            .get_mut(&id)
                            .map(|record| {
                                let output_cursor = record
                                    .startup_output
                                    .lock()
                                    .expect("startup output mutex poisoned")
                                    .next_cursor();
                                let instance = dev_instance_summary(record);
                                DebugSnapshotResult {
                                    ready: instance.status == "running",
                                    instance,
                                    output_cursor,
                                    capabilities: debug_capabilities(),
                                }
                            })
                            .ok_or_else(|| format!("unknown instance_id '{id}'"));
                        let _ = reply.send(result);
                    }
                }
            }
            let settled = closing
                .keys()
                .filter(|project| {
                    !pending
                        .values()
                        .any(|start| start.project_id.as_ref() == Some(*project))
                })
                .cloned()
                .collect::<Vec<_>>();
            for project in settled {
                if let Some(state) = closing.remove(&project) {
                    let result = match state.error {
                        Some(error) => Err(error),
                        None => Ok(()),
                    };
                    for reply in state.replies {
                        let _ = reply.send(result.clone());
                    }
                }
            }
        }
        for (_, record) in records {
            if let Err(error) = stop_record(record).await {
                tracing::error!(reason = %error, "DEV actor cleanup failed");
            }
        }
    });
}

impl DebugMcpState {
    async fn instance_request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> InstanceCommand,
    ) -> Result<T, String> {
        let (reply, response) = oneshot::channel();
        self.instances
            .send(command(reply))
            .map_err(|_| "dev instance registry closed".to_string())?;
        response
            .await
            .map_err(|_| "dev instance registry dropped response".to_string())
    }
}

#[derive(Debug, Default)]
struct DevBuildProcesses {
    frontend: Option<AsyncGroupChild>,
    desktop: Option<AsyncGroupChild>,
    captures: Vec<tokio::task::JoinHandle<()>>,
}

impl DevBuildProcesses {
    async fn stop(mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        for child in [&mut self.desktop, &mut self.frontend]
            .into_iter()
            .flatten()
        {
            if let Err(error) = child.kill().await {
                errors.push(format!("DEV process cleanup failed: {error}"));
            }
        }
        for task in self.captures.drain(..) {
            if let Err(error) = task.await {
                errors.push(format!("DEV output capture did not join: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

struct DevInstanceRecord {
    project_id: Option<ProjectId>,
    instance_id: String,
    project_dir: PathBuf,
    frontend_port: u16,
    host_addr: SocketAddr,
    ui_debug_addr: SocketAddr,
    frontend_url: String,
    config_path: PathBuf,
    store_dir: PathBuf,
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    startup_output: Arc<StdMutex<BoundedDebugOutput>>,
    startup_capture_tasks: Vec<tokio::task::JoinHandle<()>>,
    child: AsyncGroupChild,
    frontend_child: AsyncGroupChild,
    started_at_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevInstanceSummary {
    instance_id: String,
    project_dir: String,
    store_dir: String,
    session_store_path: String,
    stores_ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    frontend_url: String,
    host_addr: String,
    ui_debug_addr: String,
    status: String,
    frontend_port: u16,
    started_at_ms: u64,
}

#[derive(Clone)]
struct TydeDebugMcpServer {
    state: Arc<DebugMcpState>,
    tool_router: ToolRouter<Self>,
}

impl TydeDebugMcpServer {
    fn new(state: Arc<DebugMcpState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartInstanceToolInput {
    project_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hermes: Option<DisposableHermesEnvironment>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StopInstanceToolInput {
    instance_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvaluateToolInput {
    instance_id: String,
    expression: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DebugEventsToolInput {
    instance_id: String,
    cursor: Option<u64>,
    #[schemars(range(min = 1, max = 32768))]
    max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
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
    store_dir: String,
    session_store_path: String,
    stores_ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    frontend_url: String,
    host_addr: String,
    ui_debug_addr: String,
}

fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::json(value)?]))
}

fn err_text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
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

fn repo_root_from_parts(parts: &axum::http::request::Parts) -> Option<PathBuf> {
    let repo_root_from_header = parts
        .headers
        .get(DEBUG_REPO_ROOT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if repo_root_from_header.is_some() {
        return repo_root_from_header;
    }

    let target = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| parts.uri.path());
    split_request_target(target).1
}

#[tool_router]
impl TydeDebugMcpServer {
    #[tool(
        description = "Launch a Tyde desktop dev instance with isolated ephemeral stores and hot reload disabled. An optional typed hermes input contains Hermes behind an IPv4 loopback stub. Returns canonical store and containment attestations after the typed host and UI-debug loopback endpoints are ready. Stop and restart it to pick up code changes."
    )]
    async fn tyde_dev_instance_start(
        &self,
        Parameters(input): Parameters<StartInstanceToolInput>,
        Extension(parts): Extension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, McpError> {
        let repo_root = repo_root_from_parts(&parts);
        match start_instance(&self.state, repo_root.as_deref(), input).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "Stop a previously launched Tyde dev instance.")]
    async fn tyde_dev_instance_stop(
        &self,
        Parameters(input): Parameters<StopInstanceToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match stop_instance(&self.state, &input.instance_id).await {
            Ok(summary) => ok_json(summary),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(description = "List all Tyde dev instances currently launched by this MCP server.")]
    async fn tyde_dev_instance_list(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match list_instances(&self.state).await {
            Ok(summaries) => ok_json(summaries),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Run JavaScript inside a launched Tyde dev instance frontend. The expression is used as the body of an async function, so use `return ...` when you want to return a value."
    )]
    async fn tyde_debug_evaluate(
        &self,
        Parameters(input): Parameters<EvaluateToolInput>,
    ) -> Result<CallToolResult, McpError> {
        if input.expression.trim().is_empty() {
            return Ok(err_text("expression must not be empty"));
        }
        match evaluate_instance(&self.state, input).await {
            Ok(value) => ok_json(json!({ "value": value })),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Read bounded combined process output from a launched instance. Pass the returned nextCursor to resume without rereading output; truncated is true if the requested cursor fell behind the retained window."
    )]
    async fn tyde_debug_events(
        &self,
        Parameters(input): Parameters<DebugEventsToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match debug_events(&self.state, input).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Return a non-visual snapshot of a launched instance's process status, readiness, output cursor, and supported debug capabilities."
    )]
    async fn tyde_debug_snapshot(
        &self,
        Parameters(input): Parameters<DebugSnapshotToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match debug_snapshot(&self.state, &input.instance_id).await {
            Ok(result) => ok_json(result),
            Err(err) => Ok(err_text(err)),
        }
    }

    #[tool(
        description = "Report debug capabilities explicitly. Unsupported screenshot and second-client automation are false so QA can fail closed."
    )]
    async fn tyde_debug_capabilities(
        &self,
        Parameters(_input): Parameters<EmptyToolInput>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(debug_capabilities())
    }
}

#[tool_handler]
impl ServerHandler for TydeDebugMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tyde server hosted debug MCP. Start a child Tyde dev instance with tyde_dev_instance_start; inspect process output with tyde_debug_events, take a non-visual status snapshot with tyde_debug_snapshot, and drive its frontend with tyde_debug_evaluate. Check tyde_debug_capabilities before requiring screenshots or a second client; unsupported capabilities are reported false. Dev instances are launched with hot reload disabled, so restart the instance when you want it to pick up code changes."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
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
    let project_host = Arc::new(std::sync::OnceLock::new());
    let (instances, instance_commands) = mpsc::unbounded_channel();
    let state = Arc::new(DebugMcpState {
        instances: instances.clone(),
        project_host: project_host.clone(),
    });
    std::thread::Builder::new()
        .name("tyde-debug-mcp".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build debug MCP runtime");
            runtime.block_on(async move {
                start_instance_registry(instance_commands);
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failed to create tokio debug MCP listener");
                let mcp_service: StreamableHttpService<TydeDebugMcpServer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(TydeDebugMcpServer::new(Arc::clone(&state))),
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
                    tracing::warn!("debug MCP HTTP server stopped: {err}");
                }
            });
        })
        .map_err(|err| format!("failed to spawn debug MCP server thread: {err}"))?;

    Ok(DebugMcpHandle {
        url: format!("http://{local_addr}/mcp"),
        project_host,
        instances: Some(instances),
    })
}

async fn start_instance(
    state: &Arc<DebugMcpState>,
    repo_root: Option<&Path>,
    input: StartInstanceToolInput,
) -> Result<StartInstanceResult, String> {
    let project_dir = resolve_project_dir(repo_root, &input.project_dir)?;
    let host = state
        .project_host
        .get()
        .and_then(crate::host::WeakHostHandle::upgrade)
        .ok_or_else(|| "Debug MCP host ownership is not ready".to_owned())?;
    let projects = host.list_projects().await?;
    let mut execution_project = None;
    let mut unresolved_roots = false;
    for project in projects {
        let mut matches = false;
        for root in project.root_paths() {
            match tokio::fs::canonicalize(&root.0).await {
                Ok(canonical_root) => matches |= canonical_root == project_dir,
                Err(_) => unresolved_roots = true,
            }
        }
        if matches {
            if execution_project.is_some() {
                return Err("DEV source belongs to multiple projects; remove the ambiguous root assignment before launching".to_owned());
            }
            execution_project = Some(project);
        }
    }
    if execution_project.is_none() && unresolved_roots {
        return Err("Cannot establish DEV source ownership while registered roots are unavailable; repair the unavailable root assignments before launching an unregistered source".to_owned());
    }
    let cancellation = CancellationToken::new();
    let cancel_on_drop = cancellation.clone().drop_guard();
    let result = state
        .instance_request(|reply| InstanceCommand::Start {
            project_dir,
            input,
            execution_project: execution_project.map(Box::new),
            cancellation,
            reply,
        })
        .await?;
    cancel_on_drop.disarm();
    result
}

async fn build_instance(
    project_dir: PathBuf,
    input: StartInstanceToolInput,
    execution_project: Option<Project>,
    cancellation: CancellationToken,
) -> Result<(DevInstanceRecord, StartInstanceResult), DevStartFailure> {
    let logical_project_dir = match execution_project.as_ref().map(|project| &project.source) {
        Some(ProjectSource::GitWorkbench {
            environment: protocol::WorkbenchEnvironment::Bubblewrap { logical_root, .. },
            ..
        }) => PathBuf::from(&logical_root.0),
        Some(
            ProjectSource::GitWorkbench {
                environment: protocol::WorkbenchEnvironment::Native,
                ..
            }
            | ProjectSource::Standalone { .. },
        )
        | None => project_dir.clone(),
    };
    let instance_id = Uuid::new_v4().simple().to_string();
    let frontend_port = reserve_loopback_port()?;
    let host_port = reserve_loopback_port()?;
    let ui_debug_port = reserve_loopback_port()?;
    let host_addr = loopback_addr(host_port);
    let ui_debug_addr = loopback_addr(ui_debug_port);
    let frontend_url = format!("http://127.0.0.1:{frontend_port}");
    let resolved_hermes_runtime = input
        .hermes
        .as_ref()
        .map(|_| resolve_parent_hermes_runtime_for_dev_instance())
        .transpose()?;
    let store_dir = dev_instance_store_dir(&instance_id);
    let mut startup_cleanup = DevInstanceStartupCleanup::new(store_dir.clone());
    let mut processes = DevBuildProcesses::default();
    let result: Result<_, DevStartFailure> = async {
    std::fs::create_dir_all(&store_dir).map_err(|err| {
        format!(
            "failed to create dev instance store dir {}: {err}",
            store_dir.display()
        )
    })?;
    let store_dir = std::fs::canonicalize(&store_dir).map_err(|err| {
        format!(
            "failed to resolve dev instance store dir {}: {err}",
            store_dir.display()
        )
    })?;
    seed_dev_project_store(&store_dir, &project_dir, &instance_id)?;
    let prepared_hermes = input
        .hermes
        .as_ref()
        .zip(resolved_hermes_runtime.as_ref())
        .map(|(hermes, runtime)| prepare_disposable_hermes_environment(&store_dir, hermes, runtime))
        .transpose()?;

    startup_cleanup.track_config(dev_instance_config_path(&instance_id));
    let config_path = write_dev_config(&project_dir, frontend_port, &instance_id)?;
    let mut frontend_command =
        devtools_protocol::frontend::command(&logical_project_dir, &store_dir, frontend_port)?;
    if let Some(project) = &execution_project {
        frontend_command.in_project(project.clone());
    }
    frontend_command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_dev_instance_environment(
        &mut frontend_command,
        &store_dir,
        prepared_hermes.as_ref(),
    )?;
        processes.frontend = Some(
            frontend_command
                .group_spawn()
                .await
                .map_err(|error| format!("failed to spawn dev frontend: {error}"))?,
        );
        let frontend_child = processes
            .frontend
            .as_mut()
            .ok_or_else(|| "DEV frontend ownership missing".to_owned())?;
        let startup_output = Arc::new(StdMutex::new(BoundedDebugOutput::new(
            STARTUP_LOG_TAIL_BYTES,
        )));
        let stdout = frontend_child
            .take_stdout()
            .ok_or_else(|| "failed to capture Tyde dev instance stdout".to_string())?;
        let stderr = frontend_child
            .take_stderr()
            .ok_or_else(|| "failed to capture Tyde dev instance stderr".to_string())?;
        processes.captures = vec![
            tokio::spawn(capture_startup_output(stdout, Arc::clone(&startup_output))),
            tokio::spawn(capture_startup_output(stderr, Arc::clone(&startup_output))),
        ];

        #[cfg(feature = "test-support")]
        {
            let root = project_dir.clone();
            tokio::task::spawn_blocking(move || {
                crate::project_stream::scan_test_support::run(
                    &root,
                    crate::project_stream::scan_test_support::ScanPoint::DevFrontendProbe,
                );
            })
            .await
            .map_err(|error| format!("DEV frontend probe observer failed: {error}"))?;
        }

        let readiness = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("DEV startup cancelled".to_owned()),
            result = devtools_protocol::frontend::wait_until_ready(frontend_child, frontend_port) => result,
        };
        if let Err(error) = readiness {
            return Err(with_startup_diagnostics(error, &startup_output).into());
        }
        tracing::debug!("dev frontend ready before desktop launch");
        let mut command = tauri_dev_command(&config_path)?;
        if let Some(project) = &execution_project {
            command.in_project(project.clone());
        }
        command
            .current_dir(&logical_project_dir)
            .env("TYDE_DEV_INSTANCE", "1")
            .env("TYDE_DEV_HOST_BIND_ADDR", host_addr.to_string())
            .env("TYDE_DEV_UI_DEBUG_BIND_ADDR", ui_debug_addr.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_dev_instance_environment(&mut command, &store_dir, prepared_hermes.as_ref())?;

        processes.desktop = Some(
            command
                .group_spawn()
                .await
                .map_err(|err| format!("failed to spawn Tyde dev instance: {err}"))?,
        );
        let child = processes
            .desktop
            .as_mut()
            .ok_or_else(|| "DEV desktop ownership missing".to_owned())?;

        let stdout = child
            .take_stdout()
            .ok_or_else(|| "failed to capture Tyde dev instance stdout".to_string())?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| "failed to capture Tyde dev instance stderr".to_string())?;
        processes.captures.extend([
            tokio::spawn(capture_startup_output(stdout, Arc::clone(&startup_output))),
            tokio::spawn(capture_startup_output(stderr, Arc::clone(&startup_output))),
        ]);

        let mut record = DevInstanceRecord {
            project_id: execution_project.as_ref().map(|project| project.id.clone()),
            instance_id: instance_id.clone(),
            project_dir,
            frontend_port,
            host_addr,
            ui_debug_addr,
            frontend_url: frontend_url.clone(),
            config_path,
            store_dir,
            hermes_environment: prepared_hermes.map(|prepared| prepared.attestation),
            startup_output,
            startup_capture_tasks: std::mem::take(&mut processes.captures),
            child: processes
                .desktop
                .take()
                .ok_or_else(|| "DEV desktop ownership missing".to_owned())?,
            frontend_child: processes
                .frontend
                .take()
                .ok_or_else(|| "DEV frontend ownership missing".to_owned())?,
            started_at_ms: now_ms(),
        };

        let readiness = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("DEV startup cancelled".to_owned()),
            result = wait_for_instance_ready(&mut record) => result,
        };
        if let Err(error) = readiness {
            let error = DevStartFailure::from(error);
            return match stop_record(record).await {
                Ok(_) => Err(error),
                Err(cleanup) => Err(error.with_cleanup(cleanup)),
            };
        }

        let result = StartInstanceResult {
            instance_id: instance_id.clone(),
            status: "ready",
            project_dir: record.project_dir.display().to_string(),
            store_dir: record.store_dir.display().to_string(),
            session_store_path: record.store_dir.join("sessions.json").display().to_string(),
            stores_ephemeral: true,
            hermes_environment: record.hermes_environment.clone(),
            frontend_url: frontend_url.clone(),
            host_addr: record.host_addr.to_string(),
            ui_debug_addr: record.ui_debug_addr.to_string(),
        };

        startup_cleanup.disarm();
        Ok((record, result))
    }
    .await;
    match result {
        Ok(value) => Ok(value),
        Err(mut error) => {
            if let Err(cleanup) = processes.stop().await {
                error = error.with_cleanup(cleanup);
            }
            if let Err(cleanup) = startup_cleanup.cleanup() {
                error = error.with_cleanup(cleanup);
            }
            Err(error)
        }
    }
}

fn configure_dev_instance_environment(
    command: &mut Command,
    store_dir: &Path,
    hermes: Option<&PreparedDisposableHermesEnvironment>,
) -> Result<(), String> {
    for (env, path) in dev_instance_mutable_paths(store_dir) {
        command.env(env, path);
    }
    if let Some(hermes) = hermes {
        preserve_toolchain_homes(command, &hermes.home)?;
        command
            .env(DEV_INSTANCE_HOME_ENV, &hermes.home)
            .env(DEV_INSTANCE_HERMES_HOME_ENV, &hermes.hermes_home);
        if let Some(executable) = &hermes.runtime.executable {
            command.env(DEV_INSTANCE_HERMES_EXECUTABLE_ENV, executable);
        }
        if let Some(python) = &hermes.runtime.python {
            command.env(DEV_INSTANCE_HERMES_PYTHON_ENV, python);
        }
    }
    if hermes.is_none() {
        command.env(
            CLAUDE_SECURESTORAGE_CONFIG_DIR_ENV,
            parent_claude_credential_store_dir(),
        );
        return Ok(());
    }
    command.env_remove(CLAUDE_SECURESTORAGE_CONFIG_DIR_ENV);
    for env in DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS {
        command.env_remove(env);
    }
    for (env, _) in std::env::vars_os() {
        if is_provider_environment_key(&env) {
            command.env_remove(env);
        }
    }
    for env in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(env, DEV_INSTANCE_DENY_PROXY_URL);
    }
    command
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("GOOGLE_CLOUD_DISABLE_GCE_CHECK", "true")
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1");
    Ok(())
}

fn preserve_toolchain_homes(command: &mut Command, isolated_home: &Path) -> Result<(), String> {
    let home = std::env::var_os(DEV_INSTANCE_HOME_ENV);
    let cargo_home = std::env::var_os("CARGO_HOME");
    let rustup_home = std::env::var_os("RUSTUP_HOME");
    let xdg_cache_home = std::env::var_os("XDG_CACHE_HOME");
    preserve_toolchain_homes_from(
        command,
        home.as_deref(),
        cargo_home.as_deref(),
        rustup_home.as_deref(),
    );
    preserve_trunk_tool_cache_from(isolated_home, home.as_deref(), xdg_cache_home.as_deref())?;
    #[cfg(target_os = "linux")]
    command.env("XDG_CACHE_HOME", isolated_home.join(".cache"));
    Ok(())
}

fn preserve_toolchain_homes_from(
    command: &mut Command,
    home: Option<&std::ffi::OsStr>,
    cargo_home: Option<&std::ffi::OsStr>,
    rustup_home: Option<&std::ffi::OsStr>,
) {
    if let Some(cargo_home) = cargo_home {
        command.env("CARGO_HOME", cargo_home);
    } else if let Some(home) = home {
        command.env("CARGO_HOME", PathBuf::from(home).join(".cargo"));
    }
    if let Some(rustup_home) = rustup_home {
        command.env("RUSTUP_HOME", rustup_home);
    } else if let Some(home) = home {
        command.env("RUSTUP_HOME", PathBuf::from(home).join(".rustup"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn trunk_tool_cache_paths(
    isolated_home: &Path,
    parent_home: &Path,
    parent_xdg_cache_home: Option<&std::ffi::OsStr>,
) -> (PathBuf, PathBuf) {
    #[cfg(target_os = "macos")]
    {
        let _ = parent_xdg_cache_home;
        let relative = Path::new("Library/Caches/dev.trunkrs.trunk");
        (parent_home.join(relative), isolated_home.join(relative))
    }
    #[cfg(target_os = "linux")]
    {
        let parent_cache_root = parent_xdg_cache_home
            .map(PathBuf::from)
            .unwrap_or_else(|| parent_home.join(".cache"));
        (
            parent_cache_root.join("trunk"),
            isolated_home.join(".cache/trunk"),
        )
    }
}

fn preserve_trunk_tool_cache_from(
    isolated_home: &Path,
    parent_home: Option<&std::ffi::OsStr>,
    parent_xdg_cache_home: Option<&std::ffi::OsStr>,
) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let Some(parent_home) = parent_home else {
            return Ok(());
        };
        let (source, destination) =
            trunk_tool_cache_paths(isolated_home, Path::new(parent_home), parent_xdg_cache_home);
        if !source.is_dir() {
            return Ok(());
        }
        if destination.exists() {
            let linked = std::fs::canonicalize(&destination).map_err(|error| {
                format!(
                    "failed to resolve preserved Trunk tool cache {}: {error}",
                    destination.display()
                )
            })?;
            let source = std::fs::canonicalize(&source).map_err(|error| {
                format!(
                    "failed to resolve parent Trunk tool cache {}: {error}",
                    source.display()
                )
            })?;
            return if linked == source {
                Ok(())
            } else {
                Err(format!(
                    "isolated Trunk tool cache {} does not point to the parent cache",
                    destination.display()
                ))
            };
        }
        let parent = destination
            .parent()
            .ok_or_else(|| "isolated Trunk cache path has no parent".to_owned())?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to prepare isolated Trunk cache parent {}: {error}",
                parent.display()
            )
        })?;
        std::os::unix::fs::symlink(&source, &destination).map_err(|error| {
            format!(
                "failed to preserve Trunk tool cache {} at {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = (isolated_home, parent_home, parent_xdg_cache_home);
    Ok(())
}

fn resolve_parent_hermes_runtime_for_dev_instance()
-> Result<devtools_protocol::ResolvedHermesRuntime, String> {
    let home = std::env::var_os(DEV_INSTANCE_HOME_ENV);
    let executable = std::env::var_os(DEV_INSTANCE_HERMES_EXECUTABLE_ENV);
    let python = std::env::var_os(DEV_INSTANCE_HERMES_PYTHON_ENV);
    resolve_parent_hermes_runtime(
        home.as_deref(),
        process_env::resolved_child_process_path(),
        executable.as_deref(),
        python.as_deref(),
    )
}

fn seed_dev_project_store(
    store_dir: &Path,
    project_dir: &Path,
    instance_id: &str,
) -> Result<(), String> {
    let project_id = ProjectId(
        Uuid::parse_str(instance_id)
            .map_err(|err| format!("invalid dev instance id '{instance_id}': {err}"))?
            .to_string(),
    );
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
    state: &Arc<DebugMcpState>,
    instance_id: &str,
) -> Result<DevInstanceSummary, String> {
    state
        .instance_request(|reply| InstanceCommand::Remove {
            id: instance_id.to_owned(),
            reply,
        })
        .await?
}

async fn stop_record(mut record: DevInstanceRecord) -> Result<DevInstanceSummary, String> {
    let mut errors = Vec::new();
    if let Err(error) = record.child.kill().await {
        errors.push(format!("failed to stop dev desktop: {error}"));
    }
    if let Err(error) = record.frontend_child.kill().await {
        errors.push(format!("failed to stop dev frontend: {error}"));
    }
    for task in record.startup_capture_tasks.drain(..) {
        if let Err(error) = task.await {
            errors.push(format!("DEV output capture did not join: {error}"));
        }
    }
    let mut files = DevInstanceStartupCleanup::new(record.store_dir.clone());
    files.track_config(record.config_path.clone());
    if let Err(error) = files.cleanup() {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(dev_instance_summary(&mut record))
    } else {
        Err(errors.join("; "))
    }
}

async fn list_instances(state: &Arc<DebugMcpState>) -> Result<Vec<DevInstanceSummary>, String> {
    state
        .instance_request(|reply| InstanceCommand::List { reply })
        .await
}

async fn evaluate_instance(
    state: &Arc<DebugMcpState>,
    input: EvaluateToolInput,
) -> Result<Value, String> {
    let ui_debug_addr = state
        .instance_request(|reply| InstanceCommand::Address {
            id: input.instance_id,
            reply,
        })
        .await??;

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
    state: &Arc<DebugMcpState>,
    input: DebugEventsToolInput,
) -> Result<DebugEventsResult, String> {
    let max_bytes = input.max_bytes.unwrap_or(STARTUP_LOG_TAIL_BYTES);
    if !(1..=STARTUP_LOG_TAIL_BYTES).contains(&max_bytes) {
        return Err(format!(
            "max_bytes must be between 1 and {STARTUP_LOG_TAIL_BYTES}"
        ));
    }
    let output = state
        .instance_request(|reply| InstanceCommand::Output {
            id: input.instance_id.clone(),
            cursor: input.cursor,
            max_bytes,
            reply,
        })
        .await??;
    Ok(debug_events_result(input.instance_id, output))
}

fn debug_events_result(instance_id: String, output: DebugOutputSlice) -> DebugEventsResult {
    let events = (!output.output.is_empty())
        .then_some(DebugOutputEvent {
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
    state: &Arc<DebugMcpState>,
    instance_id: &str,
) -> Result<DebugSnapshotResult, String> {
    state
        .instance_request(|reply| InstanceCommand::Snapshot {
            id: instance_id.to_owned(),
            reply,
        })
        .await?
}

async fn wait_for_instance_ready(record: &mut DevInstanceRecord) -> Result<(), String> {
    loop {
        match record.frontend_child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err(with_startup_diagnostics(
                    format!("dev frontend exited before desktop ready: {status}"),
                    &record.startup_output,
                ));
            }
            Err(error) => {
                return Err(with_startup_diagnostics(
                    format!("failed to inspect dev frontend: {error}"),
                    &record.startup_output,
                ));
            }
        }
        match record.child.try_wait() {
            Ok(Some(exit_status)) => {
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

fn dev_instance_summary(record: &mut DevInstanceRecord) -> DevInstanceSummary {
    let status = match record.child.try_wait() {
        Ok(Some(exit_status)) => format!("exited({exit_status})"),
        Ok(None) => "running".to_string(),
        Err(err) => format!("status_error({err})"),
    };
    DevInstanceSummary {
        instance_id: record.instance_id.clone(),
        project_dir: record.project_dir.display().to_string(),
        store_dir: record.store_dir.display().to_string(),
        session_store_path: record.store_dir.join("sessions.json").display().to_string(),
        stores_ephemeral: true,
        hermes_environment: record.hermes_environment.clone(),
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

fn dev_instance_store_dir(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tyde-dev-instance-{instance_id}"))
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
    json["build"]["beforeDevCommand"] = Value::Null;
    json["build"]["devUrl"] = Value::String(format!("http://127.0.0.1:{frontend_port}"));

    let output_path = dev_instance_config_path(instance_id);
    std::fs::write(
        &output_path,
        serde_json::to_vec_pretty(&json)
            .map_err(|err| format!("failed to serialize dev config override: {err}"))?,
    )
    .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;
    Ok(output_path)
}

fn dev_instance_config_path(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tyde-dev-instance-{instance_id}.json"))
}

fn tauri_dev_command(config_path: &Path) -> Result<Command, String> {
    let cargo_tauri = process_env::find_executable_in_path("cargo-tauri").ok_or_else(|| {
        "cargo-tauri was not found in the resolved child-process PATH; install the Tauri CLI and ensure cargo-tauri is available before starting a Tyde dev instance (the launcher does not use npx or install packages)".to_string()
    })?;
    tauri_dev_command_with_cli(config_path, &cargo_tauri)
}

fn tauri_dev_command_with_cli(config_path: &Path, cargo_tauri: &Path) -> Result<Command, String> {
    let mut command = crate::process_env::command(cargo_tauri)?;
    command.arg("dev");
    command.arg("--config").arg(config_path).arg("--no-watch");
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    Ok(command)
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

fn with_startup_diagnostics(message: String, output: &StdMutex<BoundedDebugOutput>) -> String {
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
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

async fn healthz_handler() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}
