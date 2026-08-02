use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{Json, Router, response::IntoResponse, routing::get};
use client::ClientConfig;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use devtools_protocol::{
    BoundedDebugOutput, DEV_INSTANCE_DENY_PROXY_URL, DEV_INSTANCE_HERMES_EXECUTABLE_ENV,
    DEV_INSTANCE_HERMES_HOME_ENV, DEV_INSTANCE_HERMES_PYTHON_ENV, DEV_INSTANCE_HOME_ENV,
    DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS, DebugOutputSlice,
    DevInstanceHermesEnvironmentAttestation, DevInstanceStartupCleanup,
    DisposableHermesEnvironment, PreparedDisposableHermesEnvironment, UiDebugRequest,
    UiDebugResponse, dev_instance_mutable_paths, is_provider_environment_key,
    prepare_disposable_hermes_environment, resolve_parent_hermes_runtime,
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
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::process_env;

pub const DEBUG_REPO_ROOT_HEADER: &str = "x-tyde-debug-repo-root";
const START_TIMEOUT: Duration = Duration::from_secs(105);
const STARTUP_LOG_TAIL_BYTES: usize = 32 * 1024;
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";
const DEV_VOICE_PROVIDER_ENV: &str = "TYDE_DEV_VOICE_PROVIDER";
const DEV_VOICE_PROVIDER_MOCK: &str = "mock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DevInstanceVoiceProvider {
    Mock,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DevInstanceVoiceEnvironment {
    provider: DevInstanceVoiceProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevInstanceVoiceEnvironmentAttestation {
    provider: DevInstanceVoiceProvider,
    mock_backend: bool,
    media: &'static str,
    provider_credentials_stripped: bool,
}

fn voice_environment_attestation(
    voice: &DevInstanceVoiceEnvironment,
) -> DevInstanceVoiceEnvironmentAttestation {
    DevInstanceVoiceEnvironmentAttestation {
        provider: voice.provider,
        mock_backend: true,
        media: "str0m",
        provider_credentials_stripped: true,
    }
}

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
    store_dir: PathBuf,
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    voice_environment: Option<DevInstanceVoiceEnvironmentAttestation>,
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
    store_dir: String,
    session_store_path: String,
    stores_ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice_environment: Option<DevInstanceVoiceEnvironmentAttestation>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    voice: Option<DevInstanceVoiceEnvironment>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    voice_environment: Option<DevInstanceVoiceEnvironmentAttestation>,
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
        description = "Launch a Tyde desktop dev instance with isolated ephemeral stores and hot reload disabled. An optional typed hermes input contains Hermes behind an IPv4 loopback stub. An optional voice.provider=mock input selects a mock chat backend and mocks only Nova while retaining real str0m WebRTC; provider credentials are stripped and provider proxy egress is denied. Returns canonical store and containment attestations after the typed host and UI-debug loopback endpoints are ready. Stop and restart it to pick up code changes."
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
    })
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
    let resolved_hermes_runtime = input
        .hermes
        .as_ref()
        .map(|_| resolve_parent_hermes_runtime_for_dev_instance())
        .transpose()?;
    let store_dir = dev_instance_store_dir(&instance_id);
    let mut startup_cleanup = DevInstanceStartupCleanup::new(store_dir.clone());
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
    let voice_environment = input.voice.as_ref().map(voice_environment_attestation);

    startup_cleanup.track_config(dev_instance_config_path(&instance_id));
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
    configure_dev_instance_environment(
        &mut command,
        &store_dir,
        prepared_hermes.as_ref(),
        input.voice.as_ref(),
    )?;

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
        hermes_environment: prepared_hermes.map(|prepared| prepared.attestation),
        voice_environment,
        startup_output,
        startup_capture_tasks,
        child,
        started_at_ms: now_ms(),
    };

    if let Err(err) = wait_for_instance_ready(&mut record).await {
        let _ = record.child.kill().await;
        return Err(err);
    }

    let result = StartInstanceResult {
        instance_id: instance_id.clone(),
        status: "ready",
        project_dir: record.project_dir.display().to_string(),
        store_dir: record.store_dir.display().to_string(),
        session_store_path: record.store_dir.join("sessions.json").display().to_string(),
        stores_ephemeral: true,
        hermes_environment: record.hermes_environment.clone(),
        voice_environment: record.voice_environment.clone(),
        frontend_url: frontend_url.clone(),
        host_addr: record.host_addr.to_string(),
        ui_debug_addr: record.ui_debug_addr.to_string(),
    };

    let previous = state.instances.lock().await.insert(instance_id, record);
    assert!(previous.is_none(), "duplicate dev instance id inserted");
    startup_cleanup.disarm();

    Ok(result)
}

fn configure_dev_instance_environment(
    command: &mut Command,
    store_dir: &Path,
    hermes: Option<&PreparedDisposableHermesEnvironment>,
    voice: Option<&DevInstanceVoiceEnvironment>,
) -> Result<(), String> {
    for (env, path) in dev_instance_mutable_paths(store_dir) {
        command.env(env, path);
    }
    command.env_remove(DEV_VOICE_PROVIDER_ENV);
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
    } else if voice.is_some() {
        preserve_toolchain_homes(command, store_dir)?;
        command.env(DEV_INSTANCE_HOME_ENV, store_dir);
    }
    if let Some(voice) = voice {
        match voice.provider {
            DevInstanceVoiceProvider::Mock => {
                command.env(DEV_VOICE_PROVIDER_ENV, DEV_VOICE_PROVIDER_MOCK);
            }
        }
    }
    if hermes.is_none() && voice.is_none() {
        return Ok(());
    }
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

async fn list_instances(state: &Arc<DebugMcpState>) -> Result<Vec<DevInstanceSummary>, String> {
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
    let output = {
        let instances = state.instances.lock().await;
        let record = instances
            .get(&input.instance_id)
            .ok_or_else(|| format!("unknown instance_id '{}'", input.instance_id))?;
        record
            .startup_output
            .lock()
            .expect("startup output mutex poisoned")
            .read(input.cursor, max_bytes)
    };
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
    DevInstanceSummary {
        instance_id: record.instance_id.clone(),
        project_dir: record.project_dir.display().to_string(),
        store_dir: record.store_dir.display().to_string(),
        session_store_path: record.store_dir.join("sessions.json").display().to_string(),
        stores_ephemeral: true,
        hermes_environment: record.hermes_environment.clone(),
        voice_environment: record.voice_environment.clone(),
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
    Ok(tauri_dev_command_with_cli(config_path, &cargo_tauri))
}

fn tauri_dev_command_with_cli(config_path: &Path, cargo_tauri: &Path) -> Command {
    let mut command = Command::new(cargo_tauri);
    command.arg("dev");
    command.arg("--config").arg(config_path).arg("--no-watch");
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    command
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

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
            Value::String(format!(
                "node '{}' serve --port 17777 --no-autoreload",
                repo_root.join("tools/trunk-command.mjs").display()
            ))
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
        let command =
            tauri_dev_command_with_cli(&config_path, Path::new("/toolchain/bin/cargo-tauri"));
        let command = command.as_std();

        assert_eq!(
            command.get_program(),
            Path::new("/toolchain/bin/cargo-tauri").as_os_str()
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("dev"),
                std::ffi::OsStr::new("--config"),
                config_path.as_os_str(),
                std::ffi::OsStr::new("--no-watch"),
            ],
            "exact executable and arguments must preclude npx or install-capable fallbacks"
        );
    }

    #[test]
    fn tauri_dev_command_sets_resolved_path() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        let config_path = repo_root.join("frontend/tauri-shell/tauri.conf.json");
        let command =
            tauri_dev_command_with_cli(&config_path, Path::new("/toolchain/bin/cargo-tauri"));
        let command_path = command
            .as_std()
            .get_envs()
            .find_map(|(key, value)| (key == "PATH").then_some(value))
            .flatten()
            .expect("tauri dev command should set PATH");
        let resolved_path = process_env::resolved_child_process_path()
            .expect("resolved child process PATH should be available");

        assert_eq!(command_path, resolved_path);

        if let Some(home) = std::env::var_os("HOME") {
            let cargo_bin = PathBuf::from(home).join(".cargo").join("bin");
            assert!(
                std::env::split_paths(command_path).any(|path| path == cargo_bin),
                "expected resolved PATH to include {}, got {:?}",
                cargo_bin.display(),
                command_path
            );
        }
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
        configure_dev_instance_environment(&mut command, store_dir, None, None)
            .expect("configure default dev environment");
        let configured = command.as_std().get_envs().collect::<HashMap<_, _>>();

        for entry in devtools_protocol::DEV_INSTANCE_MUTABLE_PATHS {
            let expected_path = store_dir.join(entry.relative_path);
            assert_eq!(
                configured
                    .get(std::ffi::OsStr::new(entry.env))
                    .copied()
                    .flatten(),
                Some(expected_path.as_os_str()),
                "launcher did not isolate {}",
                entry.env
            );
        }
        assert!(
            !configured.contains_key(std::ffi::OsStr::new(DEV_INSTANCE_HOME_ENV)),
            "default dev instance must inherit HOME"
        );
        assert!(
            !configured.contains_key(std::ffi::OsStr::new(DEV_INSTANCE_HERMES_HOME_ENV)),
            "default dev instance must inherit HERMES_HOME"
        );
    }

    #[test]
    fn typed_voice_option_accepts_only_mock_provider() {
        let parsed: StartInstanceToolInput = serde_json::from_value(json!({
            "project_dir": "/repo",
            "voice": {"provider": "mock"}
        }))
        .expect("typed mock voice option");
        assert_eq!(
            parsed.voice.expect("voice option").provider,
            DevInstanceVoiceProvider::Mock
        );
        assert!(
            serde_json::from_value::<StartInstanceToolInput>(json!({
                "project_dir": "/repo",
                "voice": {"provider": "mock", "endpoint": "https://example.invalid"}
            }))
            .is_err(),
            "nested voice fields must fail closed"
        );
        assert!(
            serde_json::from_value::<StartInstanceToolInput>(json!({
                "project_dir": "/repo",
                "voiceProvider": "mock"
            }))
            .is_err(),
            "the duplicated launch contract must remain typed"
        );
    }

    #[test]
    fn mock_voice_environment_strips_providers_and_attests_real_media() {
        let store = tempfile::tempdir().expect("store dir");
        let voice = DevInstanceVoiceEnvironment {
            provider: DevInstanceVoiceProvider::Mock,
        };
        let mut command = Command::new("tyde");
        configure_dev_instance_environment(&mut command, store.path(), None, Some(&voice))
            .expect("configure mock voice environment");
        let configured = command.as_std().get_envs().collect::<HashMap<_, _>>();

        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_VOICE_PROVIDER_ENV))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new(DEV_VOICE_PROVIDER_MOCK))
        );
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ] {
            assert_eq!(
                configured.get(std::ffi::OsStr::new(key)).copied(),
                Some(None),
                "{key} must not reach mock voice mode"
            );
        }
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("AWS_EC2_METADATA_DISABLED"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("true"))
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("HTTPS_PROXY"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new(DEV_INSTANCE_DENY_PROXY_URL))
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_INSTANCE_HOME_ENV))
                .copied()
                .flatten(),
            Some(store.path().as_os_str()),
            "voice-only mode must not expose credential files from the parent home"
        );
        assert_eq!(
            voice_environment_attestation(&voice),
            DevInstanceVoiceEnvironmentAttestation {
                provider: DevInstanceVoiceProvider::Mock,
                mock_backend: true,
                media: "str0m",
                provider_credentials_stripped: true,
            }
        );
    }

    #[test]
    fn start_input_schema_keeps_voice_option_closed_and_optional() {
        let schema = serde_json::to_value(schemars::schema_for!(StartInstanceToolInput))
            .expect("serialize start schema");
        // Cargo.lock resolves rmcp's schema API to schemars 1.2.1. Its
        // allow_null source appends <()>::json_schema to anyOf, and its unit
        // primitive is defined by simple_impl!(() => "null"), which serializes
        // as {"type":"null"}; the canonical check refuted const:null here.
        let voice_option = schema["properties"]["voice"]
            .get("anyOf")
            .and_then(serde_json::Value::as_array)
            .expect("optional voice schema must use anyOf");
        assert_eq!(voice_option.len(), 2, "voice must have one value and null");
        let non_null: Vec<_> = voice_option
            .iter()
            .filter(|branch| branch.get("$ref").is_some())
            .collect();
        assert_eq!(non_null.len(), 1, "voice must have exactly one ref branch");
        assert_eq!(
            non_null[0],
            &json!({"$ref": "#/$defs/DevInstanceVoiceEnvironment"})
        );
        let nullable: Vec<_> = voice_option
            .iter()
            .filter(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
            .collect();
        assert_eq!(nullable.len(), 1, "voice must have exactly one null branch");
        assert_eq!(nullable[0], &json!({"type": "null"}));

        let voice = &schema["$defs"]["DevInstanceVoiceEnvironment"];
        assert_eq!(voice["additionalProperties"], json!(false));
        assert_eq!(
            voice["properties"]["provider"]["$ref"],
            json!("#/$defs/DevInstanceVoiceProvider")
        );
        assert_eq!(
            schema["$defs"]["DevInstanceVoiceProvider"]["enum"],
            json!(["mock"])
        );
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["project_dir"]));
    }

    #[test]
    fn disposable_hermes_environment_is_attested_and_denies_provider_egress() {
        let store = tempfile::tempdir().expect("store dir");
        let runtime = devtools_protocol::ResolvedHermesRuntime {
            executable: Some(std::env::current_exe().expect("current executable")),
            ..devtools_protocol::ResolvedHermesRuntime::default()
        };
        let prepared = prepare_disposable_hermes_environment(
            store.path(),
            &DisposableHermesEnvironment {
                profiles: vec!["qa".to_owned()],
                loopback_stub: devtools_protocol::HermesLoopbackStub {
                    base_url: "http://127.0.0.1:43123/v1".to_owned(),
                    model: "tyde-stub".to_owned(),
                },
            },
            &runtime,
        )
        .expect("prepare Hermes environment");
        let mut command = Command::new("tyde");
        configure_dev_instance_environment(&mut command, store.path(), Some(&prepared), None)
            .expect("configure Hermes environment");
        let configured = command.as_std().get_envs().collect::<HashMap<_, _>>();

        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_INSTANCE_HOME_ENV))
                .copied()
                .flatten(),
            Some(prepared.home.as_os_str())
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_INSTANCE_HERMES_HOME_ENV))
                .copied()
                .flatten(),
            Some(prepared.hermes_home.as_os_str())
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("HTTPS_PROXY"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new(DEV_INSTANCE_DENY_PROXY_URL))
        );
        for key in [
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "GH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AZURE_OPENAI_ENDPOINT",
        ] {
            assert_eq!(
                configured.get(std::ffi::OsStr::new(key)).copied(),
                Some(None),
                "{key} must not reach the contained process"
            );
        }
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("NO_PROXY"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("127.0.0.1"))
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("no_proxy"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("127.0.0.1"))
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new("AWS_EC2_METADATA_DISABLED"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("true"))
        );
        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_INSTANCE_HERMES_EXECUTABLE_ENV))
                .copied()
                .flatten(),
            prepared.runtime.executable.as_deref().map(Path::as_os_str)
        );
        assert!(prepared.attestation.hermes_home_ephemeral);
        assert_eq!(
            prepared.attestation.resolved_hermes_home,
            prepared.hermes_home.display().to_string()
        );
    }

    #[test]
    fn start_input_keeps_hermes_isolation_opt_in() {
        let ordinary: StartInstanceToolInput =
            serde_json::from_value(json!({ "project_dir": "/repo" })).expect("ordinary input");
        assert!(ordinary.hermes.is_none());

        let contained: StartInstanceToolInput = serde_json::from_value(json!({
            "project_dir": "/repo",
            "hermes": {
                "profiles": ["qa"],
                "loopbackStub": {
                    "baseUrl": "http://127.0.0.1:43123/v1",
                    "model": "tyde-stub"
                }
            }
        }))
        .expect("contained input");
        assert_eq!(contained.hermes.expect("Hermes input").profiles, vec!["qa"]);

        let missing_stub = serde_json::from_value::<StartInstanceToolInput>(json!({
            "project_dir": "/repo",
            "hermes": { "profiles": ["qa"] }
        }))
        .expect_err("Hermes containment without a stub must fail closed");
        assert!(missing_stub.to_string().contains("loopbackStub"));
    }

    #[test]
    fn toolchain_homes_survive_disposable_home_redirection() {
        let mut derived = Command::new("tyde");
        preserve_toolchain_homes_from(
            &mut derived,
            Some(std::ffi::OsStr::new("/parent")),
            None,
            None,
        );
        let derived = derived.as_std().get_envs().collect::<HashMap<_, _>>();
        assert_eq!(
            derived
                .get(std::ffi::OsStr::new("CARGO_HOME"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("/parent/.cargo"))
        );
        assert_eq!(
            derived
                .get(std::ffi::OsStr::new("RUSTUP_HOME"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("/parent/.rustup"))
        );

        let mut explicit = Command::new("tyde");
        preserve_toolchain_homes_from(
            &mut explicit,
            Some(std::ffi::OsStr::new("/parent")),
            Some(std::ffi::OsStr::new("/toolchain/cargo")),
            Some(std::ffi::OsStr::new("/toolchain/rustup")),
        );
        let explicit = explicit.as_std().get_envs().collect::<HashMap<_, _>>();
        assert_eq!(
            explicit
                .get(std::ffi::OsStr::new("CARGO_HOME"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("/toolchain/cargo"))
        );
        assert_eq!(
            explicit
                .get(std::ffi::OsStr::new("RUSTUP_HOME"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("/toolchain/rustup"))
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn trunk_cache_survives_while_home_remains_isolated() {
        let parent = tempfile::tempdir().expect("parent home");
        let store = tempfile::tempdir().expect("dev store");
        let isolated_home = store.path().join("home");
        std::fs::create_dir(&isolated_home).expect("create isolated home");
        let (source, destination) = trunk_tool_cache_paths(&isolated_home, parent.path(), None);
        std::fs::create_dir_all(&source).expect("create parent Trunk cache");
        std::fs::write(source.join("wasm-bindgen-marker"), b"cached")
            .expect("seed parent Trunk cache");

        preserve_trunk_tool_cache_from(&isolated_home, Some(parent.path().as_os_str()), None)
            .expect("preserve Trunk cache");
        let mut command = Command::new("tyde");
        command.env(DEV_INSTANCE_HOME_ENV, &isolated_home);
        let configured = command.as_std().get_envs().collect::<HashMap<_, _>>();

        assert_eq!(
            configured
                .get(std::ffi::OsStr::new(DEV_INSTANCE_HOME_ENV))
                .copied()
                .flatten(),
            Some(isolated_home.as_os_str())
        );
        assert!(isolated_home.starts_with(store.path()));
        assert!(!source.starts_with(store.path()));
        assert_eq!(
            std::fs::canonicalize(destination).expect("resolve linked Trunk cache"),
            std::fs::canonicalize(source).expect("resolve parent Trunk cache")
        );
    }

    #[test]
    fn disposable_profile_grammar_matches_hermes_discovery() {
        for profile in ["qa", "qaverify_2", "gpt-5"] {
            assert_eq!(
                devtools_protocol::is_valid_disposable_hermes_profile_name(profile),
                crate::backend::hermes_config::is_valid_profile_name(profile)
            );
        }
        for profile in ["_qa", "-qa", "QA"] {
            assert_eq!(
                devtools_protocol::is_valid_disposable_hermes_profile_name(profile),
                crate::backend::hermes_config::is_valid_profile_name(profile)
            );
        }
        assert!(crate::backend::hermes_config::is_valid_profile_name(
            "default"
        ));
        assert!(!devtools_protocol::is_valid_disposable_hermes_profile_name(
            "default"
        ));
    }

    #[test]
    fn dev_instance_seeds_only_requested_project() {
        let store = tempfile::tempdir().expect("store dir");
        let project = tempfile::tempdir().expect("project dir");
        seed_dev_project_store(
            store.path(),
            project.path(),
            "12345678123456781234567812345678",
        )
        .expect("seed project store");
        let contents: Value = serde_json::from_slice(
            &std::fs::read(store.path().join("projects.json")).expect("read project store"),
        )
        .expect("parse project store");
        let records = contents["records"].as_object().expect("records object");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records["12345678-1234-5678-1234-567812345678"]["source"]["kind"],
            Value::String("standalone".to_owned())
        );
        assert_eq!(
            records["12345678-1234-5678-1234-567812345678"]["source"]["roots"][0],
            Value::String(project.path().display().to_string())
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
