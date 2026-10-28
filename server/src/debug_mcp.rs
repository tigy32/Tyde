use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::process_env::launch::{
    AsyncCommandGroup, AsyncGroupChild, GroupCommand as Command, ProcessStdio as Stdio,
};
use axum::{Json, Router, response::IntoResponse, routing::get};
use client::ClientConfig;
use devtools_protocol::{
    BoundedDebugOutput, CLAUDE_SECURESTORAGE_CONFIG_DIR_ENV, DEV_INSTANCE_DENY_PROXY_URL,
    DEV_INSTANCE_HERMES_EXECUTABLE_ENV, DEV_INSTANCE_HERMES_HOME_ENV,
    DEV_INSTANCE_HERMES_PYTHON_ENV, DEV_INSTANCE_HOME_ENV, DEV_INSTANCE_ID_ENV,
    DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS, DEV_INSTANCE_STARTUP_ADDR_ENV,
    DEV_INSTANCE_STARTUP_CONTRACT, DEV_INSTANCE_STARTUP_CONTRACT_PATH, DebugOutputSlice,
    DevInstanceFailureKind, DevInstanceHermesEnvironmentAttestation, DevInstanceId,
    DevInstanceStartupCleanup, DevInstanceStartupContract, DevInstanceStartupNotification,
    DevInstanceStartupPhase, DevInstanceStartupSignal, DevInstanceStatus,
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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
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
        reply: oneshot::Sender<Result<DevInstanceSummary, String>>,
    },
    Wait {
        id: DevInstanceId,
        reply: oneshot::Sender<Result<DevInstanceSummary, String>>,
    },
    CloseProject {
        project_id: ProjectId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Remove {
        id: DevInstanceId,
        reply: oneshot::Sender<Result<DevInstanceSummary, String>>,
    },
    List {
        reply: oneshot::Sender<Vec<DevInstanceSummary>>,
    },
    Address {
        id: DevInstanceId,
        reply: oneshot::Sender<Result<SocketAddr, String>>,
    },
    Output {
        id: DevInstanceId,
        cursor: Option<u64>,
        max_bytes: usize,
        reply: oneshot::Sender<Result<DebugOutputSlice, String>>,
    },
    Snapshot {
        id: DevInstanceId,
        reply: oneshot::Sender<Result<DebugSnapshotResult, String>>,
    },
}

struct DevAttempt {
    instance_id: DevInstanceId,
    project_id: Option<ProjectId>,
    project_dir: PathBuf,
    started_at_ms: u64,
    status: DevInstanceStatus,
    cancellation: CancellationToken,
    output: StartupOutput,
    waiters: Vec<oneshot::Sender<Result<DevInstanceSummary, String>>>,
    stops: Vec<oneshot::Sender<Result<DevInstanceSummary, String>>>,
}

struct StartupProgress {
    instance_id: DevInstanceId,
    phase: DevInstanceStartupPhase,
}

#[derive(Clone)]
struct StartupReporter {
    instance_id: DevInstanceId,
    updates: mpsc::UnboundedSender<StartupProgress>,
}

impl StartupReporter {
    fn phase(&self, phase: DevInstanceStartupPhase) -> Result<(), String> {
        self.updates
            .send(StartupProgress {
                instance_id: self.instance_id.clone(),
                phase,
            })
            .map_err(|_| "DEV startup owner stopped".to_owned())
    }
}

struct DevStartFailure {
    kind: DevInstanceFailureKind,
    message: String,
    cleanup_error: Option<String>,
}

impl From<String> for DevStartFailure {
    fn from(message: String) -> Self {
        Self::new(DevInstanceFailureKind::Startup, message)
    }
}

impl DevStartFailure {
    fn new(kind: DevInstanceFailureKind, message: String) -> Self {
        Self {
            kind,
            message,
            cleanup_error: None,
        }
    }

    fn cancelled() -> Self {
        Self::new(
            DevInstanceFailureKind::Cancelled,
            "DEV startup cancelled".to_owned(),
        )
    }

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
        let mut records = HashMap::<DevInstanceId, DevInstanceRecord>::new();
        let mut attempts = HashMap::<DevInstanceId, DevAttempt>::new();
        let mut closed_projects = std::collections::HashSet::new();
        let mut closing = HashMap::<ProjectId, ClosingDevProject>::new();
        let mut starts = tokio::task::JoinSet::new();
        let mut pending = HashMap::<tokio::task::Id, DevInstanceId>::new();
        let (updates, mut progress) = mpsc::unbounded_channel::<StartupProgress>();
        let mut receiving = true;
        loop {
            let command = tokio::select! {
                biased;
                Some(update) = progress.recv() => {
                    if let Some(attempt) = attempts.get_mut(&update.instance_id)
                        && matches!(attempt.status, DevInstanceStatus::Starting { .. }) {
                        attempt.status = DevInstanceStatus::Starting { phase: update.phase };
                    }
                    None
                },
                completed = starts.join_next_with_id(), if !starts.is_empty() => {
                    if let Some(completed) = completed {
                        let (task_id, result) = match completed {
                            Ok(value) => value,
                            Err(error) => (error.id(), Err(DevStartFailure::new(
                                DevInstanceFailureKind::TaskFailed, "DEV startup task failed".to_owned(),
                            ).with_cleanup("DEV startup cleanup could not be confirmed".to_owned()))),
                        };
                        if let Some(id) = pending.remove(&task_id)
                            && let Some(attempt) = attempts.get_mut(&id) {
                            let result = match result {
                                Ok(record) if attempt.cancellation.is_cancelled() => {
                                    let error = DevStartFailure::cancelled();
                                    match stop_record(record).await {
                                        Ok(()) => Err(error),
                                        Err(cleanup) => Err(error.with_cleanup(cleanup)),
                                    }
                                },
                                other => other,
                            };
                            match result {
                                Ok(record) => {
                                    records.insert(id.clone(), record);
                                    attempt.status = DevInstanceStatus::Ready;
                                },
                                Err(error) => {
                                    if let Some(project) = &attempt.project_id
                                        && let Some(state) = closing.get_mut(project)
                                        && let Some(cleanup) = &error.cleanup_error {
                                            state.error = Some(cleanup.clone());
                                        }
                                    attempt.status = match attempt.status {
                                        DevInstanceStatus::Starting { phase } => {
                                            tracing::warn!(phase = ?phase, "DEV startup failed; inspect its typed instance status");
                                            DevInstanceStatus::Failed {
                                                phase,
                                                failure: error.kind,
                                                error: error.message,
                                                cleanup_error: error.cleanup_error,
                                            }
                                        },
                                        _ => DevInstanceStatus::StatusError {
                                            error: "DEV startup completed outside its starting state".to_owned(),
                                        },
                                    };
                                },
                            }
                            let summary = dev_instance_summary(attempt, records.get_mut(&id));
                            for waiter in attempt.waiters.drain(..) {
                                let _ = waiter.send(Ok(summary.clone()));
                            }
                            if !attempt.stops.is_empty() {
                                let cleanup_error = match &attempt.status {
                                    DevInstanceStatus::Failed { cleanup_error, .. } => cleanup_error.clone(),
                                    _ => None,
                                };
                                let mut stopped = summary.clone();
                                stopped.status = DevInstanceStatus::Stopped;
                                stopped.ready = None;
                                for reply in attempt.stops.drain(..) {
                                    let result = match &cleanup_error {
                                        Some(error) => Err(error.clone()),
                                        None => Ok(stopped.clone()),
                                    };
                                    let _ = reply.send(result);
                                }
                                if cleanup_error.is_none() {
                                    attempts.remove(&id);
                                }
                            }
                        }
                    }
                    None
                },
                command = receiver.recv(), if receiving => match command {
                    Some(command) => Some(command),
                    None => {
                        receiving = false;
                        for attempt in attempts.values() {
                            attempt.cancellation.cancel();
                        }
                        None
                    },
                },
                else => break,
            };
            if let Some(command) = command {
                match command {
                    InstanceCommand::Start {
                        project_dir,
                        input,
                        execution_project,
                        reply,
                    } => {
                        let project_id =
                            execution_project.as_ref().map(|project| project.id.clone());
                        if project_id
                            .as_ref()
                            .is_some_and(|project| closed_projects.contains(project))
                        {
                            let _ = reply.send(Err(
                                "Workbench is being removed; no DEV process was launched"
                                    .to_owned(),
                            ));
                        } else if let Some(existing) = attempts
                            .values()
                            .find(|attempt| attempt.project_dir == project_dir)
                        {
                            let _ = reply.send(Err(format!("DEV source already has an attempt ({}); observe or await it instead of launching again", existing.instance_id.0)));
                        } else {
                            let instance_id = DevInstanceId(Uuid::new_v4().simple().to_string());
                            let cancellation = CancellationToken::new();
                            let output = StartupOutput::new();
                            let reporter = StartupReporter {
                                instance_id: instance_id.clone(),
                                updates: updates.clone(),
                            };
                            let mut attempt = DevAttempt {
                                instance_id: instance_id.clone(),
                                project_id,
                                project_dir: project_dir.clone(),
                                started_at_ms: now_ms(),
                                status: DevInstanceStatus::Starting {
                                    phase: DevInstanceStartupPhase::Preparing,
                                },
                                cancellation: cancellation.clone(),
                                output: output.clone(),
                                waiters: Vec::new(),
                                stops: Vec::new(),
                            };
                            let summary = dev_instance_summary(&mut attempt, None);
                            attempts.insert(instance_id.clone(), attempt);
                            let task = starts.spawn(build_instance(
                                project_dir,
                                input,
                                execution_project.map(|project| *project),
                                cancellation,
                                output,
                                reporter,
                            ));
                            pending.insert(task.id(), instance_id);
                            // Losing this reply does not withdraw the actor's process ownership.
                            let _ = reply.send(Ok(summary));
                        }
                    }
                    InstanceCommand::Wait { id, reply } => match attempts.get_mut(&id) {
                        Some(attempt)
                            if matches!(attempt.status, DevInstanceStatus::Starting { .. }) =>
                        {
                            attempt.waiters.retain(|waiter| !waiter.is_closed());
                            attempt.waiters.push(reply);
                        }
                        Some(attempt) => {
                            let _ =
                                reply.send(Ok(dev_instance_summary(attempt, records.get_mut(&id))));
                        }
                        None => {
                            let _ = reply.send(Err("Unknown DEV instance".to_owned()));
                        }
                    },
                    InstanceCommand::CloseProject { project_id, reply } => {
                        closed_projects.insert(project_id.clone());
                        closing
                            .entry(project_id.clone())
                            .or_default()
                            .replies
                            .push(reply);
                        let ids = attempts
                            .iter()
                            .filter(|(_, attempt)| attempt.project_id.as_ref() == Some(&project_id))
                            .map(|(id, _)| id.clone())
                            .collect::<Vec<_>>();
                        for id in ids {
                            if let Some(attempt) = attempts.get(&id) {
                                attempt.cancellation.cancel();
                                let cleanup_error = match &attempt.status {
                                    DevInstanceStatus::Failed { cleanup_error, .. } => {
                                        cleanup_error.as_ref()
                                    }
                                    DevInstanceStatus::CleanupFailed { error } => Some(error),
                                    _ => None,
                                };
                                if let Some(error) = cleanup_error {
                                    closing.entry(project_id.clone()).or_default().error =
                                        Some(error.clone());
                                }
                            }
                            if let Some(record) = records.remove(&id) {
                                match stop_record(record).await {
                                    Ok(()) => {
                                        if let Some(attempt) = attempts.get_mut(&id) {
                                            attempt.status = DevInstanceStatus::Stopped;
                                        }
                                    }
                                    Err(error) => {
                                        if let Some(attempt) = attempts.get_mut(&id) {
                                            attempt.status = DevInstanceStatus::CleanupFailed {
                                                error: error.clone(),
                                            };
                                        }
                                        closing.entry(project_id.clone()).or_default().error =
                                            Some(error);
                                    }
                                }
                            }
                        }
                    }
                    InstanceCommand::Remove { id, reply } => match attempts.get_mut(&id) {
                        Some(attempt)
                            if matches!(attempt.status, DevInstanceStatus::Starting { .. }) =>
                        {
                            attempt.cancellation.cancel();
                            attempt.stops.push(reply);
                        }
                        Some(attempt) => {
                            let mut summary = dev_instance_summary(attempt, records.get_mut(&id));
                            summary.status = DevInstanceStatus::Stopped;
                            summary.ready = None;
                            let result = match records.remove(&id) {
                                Some(record) => stop_record(record).await.map(|()| summary),
                                None => match &attempt.status {
                                    DevInstanceStatus::Failed {
                                        cleanup_error: Some(error),
                                        ..
                                    }
                                    | DevInstanceStatus::CleanupFailed { error } => {
                                        Err(error.clone())
                                    }
                                    _ => Ok(summary),
                                },
                            };
                            if let Err(error) = &result {
                                attempt.status = DevInstanceStatus::CleanupFailed {
                                    error: error.clone(),
                                };
                            }
                            if result.is_ok() {
                                attempts.remove(&id);
                            }
                            let _ = reply.send(result);
                        }
                        None => {
                            let _ = reply.send(Err("Unknown DEV instance".to_owned()));
                        }
                    },
                    InstanceCommand::List { reply } => {
                        let mut summaries = attempts
                            .iter_mut()
                            .map(|(id, attempt)| dev_instance_summary(attempt, records.get_mut(id)))
                            .collect::<Vec<_>>();
                        summaries
                            .sort_by(|left, right| left.instance_id.0.cmp(&right.instance_id.0));
                        let _ = reply.send(summaries);
                    }
                    InstanceCommand::Address { id, reply } => {
                        let result = match attempts.get_mut(&id) {
                            Some(attempt) => {
                                let summary = dev_instance_summary(attempt, records.get_mut(&id));
                                match (summary.status, records.get(&id)) {
                                    (DevInstanceStatus::Ready, Some(record)) => {
                                        Ok(record.ui_debug_addr)
                                    }
                                    _ => Err("DEV instance is not ready; inspect its typed status"
                                        .to_owned()),
                                }
                            }
                            None => Err("Unknown DEV instance".to_owned()),
                        };
                        let _ = reply.send(result);
                    }
                    InstanceCommand::Output {
                        id,
                        cursor,
                        max_bytes,
                        reply,
                    } => {
                        let result = match attempts.get(&id) {
                            Some(attempt) => attempt.output.read(cursor, max_bytes).await,
                            None => Err("Unknown DEV instance".to_owned()),
                        };
                        let _ = reply.send(result);
                    }
                    InstanceCommand::Snapshot { id, reply } => {
                        let result = match attempts.get_mut(&id) {
                            Some(attempt) => match attempt.output.head().await {
                                Ok(output_cursor) => {
                                    let instance =
                                        dev_instance_summary(attempt, records.get_mut(&id));
                                    Ok(DebugSnapshotResult {
                                        ready: matches!(instance.status, DevInstanceStatus::Ready),
                                        instance,
                                        output_cursor,
                                        capabilities: debug_capabilities(),
                                    })
                                }
                                Err(error) => Err(error),
                            },
                            None => Err("Unknown DEV instance".to_owned()),
                        };
                        let _ = reply.send(result);
                    }
                }
            }
            let settled = closing
                .keys()
                .filter(|project| {
                    !pending.values().any(|id| {
                        attempts
                            .get(id)
                            .is_some_and(|attempt| attempt.project_id.as_ref() == Some(*project))
                    })
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
            if !receiving && starts.is_empty() {
                break;
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
    captures: Vec<tokio::task::JoinHandle<Result<(), String>>>,
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
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(format!("DEV output capture did not join: {error}")),
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
    frontend_port: u16,
    frontend_build: devtools_protocol::DevFrontendAttestation,
    host_addr: SocketAddr,
    ui_debug_addr: SocketAddr,
    frontend_url: String,
    config_path: PathBuf,
    store_dir: PathBuf,
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,

    startup_capture_tasks: Vec<tokio::task::JoinHandle<Result<(), String>>>,
    child: AsyncGroupChild,
    frontend_child: AsyncGroupChild,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevInstanceSummary {
    instance_id: DevInstanceId,
    project_id: Option<ProjectId>,
    project_dir: String,
    status: DevInstanceStatus,
    started_at_ms: u64,
    #[serde(flatten)]
    ready: Option<DevInstanceReady>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevInstanceReady {
    store_dir: String,
    session_store_path: String,
    stores_ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hermes_environment: Option<DevInstanceHermesEnvironmentAttestation>,
    frontend_url: String,
    host_addr: String,
    ui_debug_addr: String,
    frontend_port: u16,
    frontend_build: devtools_protocol::DevFrontendAttestation,
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
    instance_id: DevInstanceId,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyToolInput {}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvaluateToolInput {
    instance_id: DevInstanceId,
    expression: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DebugEventsToolInput {
    instance_id: DevInstanceId,
    cursor: Option<u64>,
    #[schemars(range(min = 1, max = 32768))]
    max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DebugSnapshotToolInput {
    instance_id: DevInstanceId,
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
    instance_id: DevInstanceId,
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
        description = "Start exactly one actor-owned Tyde desktop DEV attempt with isolated ephemeral stores and hot reload disabled. Returns its starting handle immediately. Observe pending/failed state with list, events or snapshot; await this same instance_id with tyde_dev_instance_wait for typed host/UI readiness and canonical store attestations. A lost caller response never cancels the attempt. Duplicate source starts fail explicitly."
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

    #[tool(
        description = "Await this existing DEV startup until ready or failed. Reattach with the same instance_id after a lost response; dropping the wait never cancels startup. No startup deadline."
    )]
    async fn tyde_dev_instance_wait(
        &self,
        Parameters(input): Parameters<StopInstanceToolInput>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .state
            .instance_request(|reply| InstanceCommand::Wait {
                id: input.instance_id,
                reply,
            })
            .await
        {
            Ok(Ok(summary)) => match &summary.status {
                DevInstanceStatus::Ready => ok_json(summary),
                _ => Ok(CallToolResult::error(vec![Content::json(summary)?])),
            },
            Ok(Err(error)) | Err(error) => Ok(err_text(error)),
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

    #[tool(
        description = "List all DEV attempts owned by this MCP server, including starting and failed attempts. Only ready status attests typed host/UI readiness; pending attempts have no store or endpoint attestation."
    )]
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
                "Tyde server hosted debug MCP. Start a child Tyde dev instance with tyde_dev_instance_start, then await the same handle with tyde_dev_instance_wait; inspect process output with tyde_debug_events, take a non-visual status snapshot with tyde_debug_snapshot, and drive its frontend with tyde_debug_evaluate. Check tyde_debug_capabilities before requiring screenshots or a second client; unsupported capabilities are reported false. Dev instances are launched with hot reload disabled, so restart the instance when you want it to pick up code changes."
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

pub fn start_server(config: &crate::host::HostRuntimeConfig) -> Result<DebugMcpHandle, String> {
    let bind_addr = config.debug_mcp_bind_addr.unwrap_or_else(|| {
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
) -> Result<DevInstanceSummary, String> {
    let project_dir = resolve_project_dir(repo_root, &input.project_dir)?;
    let contract_bytes = tokio::fs::read(project_dir.join(DEV_INSTANCE_STARTUP_CONTRACT_PATH)).await
        .map_err(|error| format!("DEV child startup contract is unavailable: {error}; update this exact source checkout with the launcher startup contract before launching"))?;
    let contract: DevInstanceStartupContract = serde_json::from_slice(&contract_bytes)
        .map_err(|error| format!("Unsupported DEV child startup contract: {error}; update the launcher and this exact source checkout together"))?;
    if contract != DEV_INSTANCE_STARTUP_CONTRACT {
        return Err("Unsupported DEV child startup contract; update the launcher and this exact source checkout together".to_owned());
    }
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
    state
        .instance_request(|reply| InstanceCommand::Start {
            project_dir,
            input,
            execution_project: execution_project.map(Box::new),
            reply,
        })
        .await?
}

async fn build_instance(
    project_dir: PathBuf,
    input: StartInstanceToolInput,
    execution_project: Option<Project>,
    cancellation: CancellationToken,
    startup_output: StartupOutput,
    reporter: StartupReporter,
) -> Result<DevInstanceRecord, DevStartFailure> {
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
    let instance_id = reporter.instance_id.clone();
    let host_port = reserve_loopback_port()?;
    let ui_debug_port = reserve_loopback_port()?;
    let host_addr = loopback_addr(host_port);
    let ui_debug_addr = loopback_addr(ui_debug_port);
    let resolved_hermes_runtime = input
        .hermes
        .as_ref()
        .map(|_| resolve_parent_hermes_runtime_for_dev_instance())
        .transpose()?;
    let store_dir = dev_instance_store_dir(&instance_id.0);
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
    seed_dev_project_store(&store_dir, &project_dir, &instance_id.0)?;
    let prepared_hermes = input
        .hermes
        .as_ref()
        .zip(resolved_hermes_runtime.as_ref())
        .map(|(hermes, runtime)| prepare_disposable_hermes_environment(&store_dir, hermes, runtime))
        .transpose()?;

    startup_cleanup.track_config(dev_instance_config_path(&instance_id.0));
    let mut frontend_command =
        devtools_protocol::frontend::command(&logical_project_dir, &store_dir)?;

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
        reporter.phase(DevInstanceStartupPhase::Frontend)?;
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
        let stderr = frontend_child
            .take_stderr()
            .ok_or_else(|| "failed to capture Tyde dev instance stderr".to_string())?;
        processes.captures = vec![
            tokio::spawn(capture_startup_output(stderr, startup_output.clone())),
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

        let frontend_ready = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(DevStartFailure::cancelled()),
            result = devtools_protocol::frontend::wait_until_ready(frontend_child) => result.map_err(DevStartFailure::from),
        }?;
        let frontend_port = frontend_ready.port;
        let frontend_url = format!("http://127.0.0.1:{frontend_port}");
        let config_path = write_dev_config(&project_dir, frontend_port, &instance_id.0)?;
        tracing::debug!("dev frontend ready before desktop launch");
        reporter.phase(DevInstanceStartupPhase::Desktop)?;
        let startup_listener = TcpListener::bind("127.0.0.1:0").await
            .map_err(|error| format!("Cannot bind DEV startup notification endpoint: {error}"))?;
        let startup_addr = startup_listener.local_addr()
            .map_err(|error| format!("Cannot read DEV startup notification endpoint: {error}"))?;
        let mut command = tauri_dev_command(&config_path)?;
        if let Some(project) = &execution_project {
            command.in_project(project.clone());
        }
        command
            .current_dir(&logical_project_dir)
            .env("TYDE_DEV_INSTANCE", "1")
            .env(DEV_INSTANCE_STARTUP_ADDR_ENV, startup_addr.to_string())
            .env(DEV_INSTANCE_ID_ENV, &instance_id.0)
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
            tokio::spawn(capture_startup_output(stdout, startup_output.clone())),
            tokio::spawn(capture_startup_output(stderr, startup_output.clone())),
        ]);

        let mut record = DevInstanceRecord {
            frontend_port,
            frontend_build: frontend_ready.attestation,
            host_addr,
            ui_debug_addr,
            frontend_url: frontend_url.clone(),
            config_path,
            store_dir,
            hermes_environment: prepared_hermes.map(|prepared| prepared.attestation),
            startup_capture_tasks: std::mem::take(&mut processes.captures),
            child: processes
                .desktop
                .take()
                .ok_or_else(|| "DEV desktop ownership missing".to_owned())?,
            frontend_child: processes
                .frontend
                .take()
                .ok_or_else(|| "DEV frontend ownership missing".to_owned())?,
        };

        let readiness = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(DevStartFailure::cancelled()),
            result = wait_for_instance_ready(&mut record, startup_listener, &reporter) => result,
        };
        if let Err(error) = readiness {
            return match stop_record(record).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.with_cleanup(cleanup)),
            };
        }

        startup_cleanup.disarm();
        Ok(record)
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
            error.message = with_startup_diagnostics(error.message, &startup_output).await;
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
    instance_id: &DevInstanceId,
) -> Result<DevInstanceSummary, String> {
    state
        .instance_request(|reply| InstanceCommand::Remove {
            id: instance_id.clone(),
            reply,
        })
        .await?
}

async fn stop_record(mut record: DevInstanceRecord) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = record.child.kill().await {
        errors.push(format!("failed to stop dev desktop: {error}"));
    }
    if let Err(error) = record.frontend_child.kill().await {
        errors.push(format!("failed to stop dev frontend: {error}"));
    }
    for task in record.startup_capture_tasks.drain(..) {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(error) => errors.push(format!("DEV output capture did not join: {error}")),
        }
    }
    let mut files = DevInstanceStartupCleanup::new(record.store_dir.clone());
    files.track_config(record.config_path.clone());
    if let Err(error) = files.cleanup() {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
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

fn debug_events_result(instance_id: DevInstanceId, output: DebugOutputSlice) -> DebugEventsResult {
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
    instance_id: &DevInstanceId,
) -> Result<DebugSnapshotResult, String> {
    state
        .instance_request(|reply| InstanceCommand::Snapshot {
            id: instance_id.clone(),
            reply,
        })
        .await?
}

async fn wait_for_instance_ready(
    record: &mut DevInstanceRecord,
    listener: TcpListener,
    reporter: &StartupReporter,
) -> Result<(), DevStartFailure> {
    let host_addr = record.host_addr;
    let ui_debug_addr = record.ui_debug_addr;
    let readiness = async {
        let mut host_ready = false;
        let mut ui_ready = false;
        while !host_ready || !ui_ready {
            let (stream, remote) = listener
                .accept()
                .await
                .map_err(|error| format!("DEV startup notification accept failed: {error}"))?;
            if !remote.ip().is_loopback() {
                return Err("DEV startup notification was not loopback-owned"
                    .to_owned()
                    .into());
            }
            let mut body = Vec::new();
            stream
                .take(4097)
                .read_to_end(&mut body)
                .await
                .map_err(|error| format!("DEV startup notification read failed: {error}"))?;
            if body.len() > 4096 {
                return Err("DEV startup notification exceeded its bound"
                    .to_owned()
                    .into());
            }
            let notification: DevInstanceStartupNotification = serde_json::from_slice(&body)
                .map_err(|error| format!("Invalid DEV startup notification: {error}"))?;
            if notification.protocol != DEV_INSTANCE_STARTUP_CONTRACT.protocol {
                return Err("Unsupported DEV startup notification protocol"
                    .to_owned()
                    .into());
            }
            if notification.instance_id != reporter.instance_id {
                return Err("DEV startup notification ownership mismatch"
                    .to_owned()
                    .into());
            }
            match notification.signal {
                DevInstanceStartupSignal::HostListening { address } => {
                    if host_ready || address != host_addr {
                        return Err("Invalid or duplicate DEV host readiness notification"
                            .to_owned()
                            .into());
                    }
                    reporter.phase(DevInstanceStartupPhase::HostHandshake)?;
                    connect_host_endpoint(host_addr).await?;
                    host_ready = true;
                }
                DevInstanceStartupSignal::UiReady => {
                    if ui_ready {
                        return Err("Duplicate DEV UI readiness notification".to_owned().into());
                    }
                    reporter.phase(DevInstanceStartupPhase::UiHandshake)?;
                    match send_ui_debug_request(ui_debug_addr, UiDebugRequest::Ping)
                        .await
                        .map_err(|error| {
                            DevStartFailure::new(DevInstanceFailureKind::UiHandshake, error)
                        })? {
                        UiDebugResponse::Pong => ui_ready = true,
                        UiDebugResponse::Error { message } => {
                            return Err(DevStartFailure::new(
                                DevInstanceFailureKind::UiHandshake,
                                format!("DEV UI readiness failed: {message}"),
                            ));
                        }
                        _ => {
                            return Err(DevStartFailure::new(
                                DevInstanceFailureKind::UiHandshake,
                                "DEV UI readiness returned an unexpected response".to_owned(),
                            ));
                        }
                    }
                }
            }
        }
        Ok::<(), DevStartFailure>(())
    };
    tokio::select! {
        biased;
        status = record.frontend_child.wait() => match status {
            Ok(status) => Err(DevStartFailure::new(DevInstanceFailureKind::ChildExit, format!("dev frontend exited before desktop ready: {status}"))),
            Err(error) => Err(format!("failed to wait for dev frontend: {error}").into()),
        },
        status = record.child.wait() => match status {
            Ok(status) => Err(DevStartFailure::new(DevInstanceFailureKind::ChildExit, format!("dev instance exited before ready: {status}"))),
            Err(error) => Err(format!("failed to wait for dev desktop: {error}").into()),
        },
        result = readiness => result,
    }
}

fn dev_instance_summary(
    attempt: &mut DevAttempt,
    record: Option<&mut DevInstanceRecord>,
) -> DevInstanceSummary {
    if matches!(attempt.status, DevInstanceStatus::Ready) && record.is_none() {
        attempt.status = DevInstanceStatus::StatusError {
            error: "Ready DEV instance has no owned process record".to_owned(),
        };
    }
    let ready = record.map(|record| {
        if matches!(attempt.status, DevInstanceStatus::Ready) {
            for child in [&mut record.child, &mut record.frontend_child] {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        attempt.status = DevInstanceStatus::Exited {
                            exit_status: status.to_string(),
                        };
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        attempt.status = DevInstanceStatus::StatusError {
                            error: error.to_string(),
                        };
                        break;
                    }
                }
            }
        }
        DevInstanceReady {
            store_dir: record.store_dir.display().to_string(),
            session_store_path: record.store_dir.join("sessions.json").display().to_string(),
            stores_ephemeral: true,
            hermes_environment: record.hermes_environment.clone(),
            frontend_url: record.frontend_url.clone(),
            host_addr: record.host_addr.to_string(),
            ui_debug_addr: record.ui_debug_addr.to_string(),
            frontend_port: record.frontend_port,
            frontend_build: record.frontend_build.clone(),
        }
    });
    DevInstanceSummary {
        instance_id: attempt.instance_id.clone(),
        project_id: attempt.project_id.clone(),
        project_dir: attempt.project_dir.display().to_string(),
        status: attempt.status.clone(),
        started_at_ms: attempt.started_at_ms,
        ready,
    }
}

async fn connect_host_endpoint(addr: SocketAddr) -> Result<(), DevStartFailure> {
    let stream = TcpStream::connect(addr).await.map_err(|err| {
        DevStartFailure::new(
            DevInstanceFailureKind::HostHandshake,
            format!("failed to connect to host endpoint {addr}: {err}"),
        )
    })?;
    let config = ClientConfig::current();
    let _connection = client::connect(&config, stream).await.map_err(|error| {
        let kind = match &error {
            client::HandshakeError::IncompatibleProtocol { client, server } => {
                DevInstanceFailureKind::HostProtocolIncompatible {
                    client: *client,
                    server: *server,
                }
            }
            client::HandshakeError::Rejected(rejection)
                if rejection.code == protocol::RejectCode::IncompatibleProtocol =>
            {
                DevInstanceFailureKind::HostProtocolIncompatible {
                    client: config.protocol_version,
                    server: rejection.server_protocol_version,
                }
            }
            _ => DevInstanceFailureKind::HostHandshake,
        };
        DevStartFailure::new(kind, format!("host handshake failed for {addr}: {error:?}"))
    })?;
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

enum StartupOutputCommand {
    Append(Vec<u8>),
    Head(oneshot::Sender<u64>),
    Read {
        cursor: Option<u64>,
        max_bytes: usize,
        reply: oneshot::Sender<DebugOutputSlice>,
    },
}

#[derive(Clone)]
struct StartupOutput(mpsc::Sender<StartupOutputCommand>);

impl StartupOutput {
    fn new() -> Self {
        let (sender, mut commands) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut output = BoundedDebugOutput::new(STARTUP_LOG_TAIL_BYTES);
            while let Some(command) = commands.recv().await {
                match command {
                    StartupOutputCommand::Append(bytes) => output.append(&bytes),
                    StartupOutputCommand::Head(reply) => {
                        let _ = reply.send(output.next_cursor());
                    }
                    StartupOutputCommand::Read {
                        cursor,
                        max_bytes,
                        reply,
                    } => {
                        let _ = reply.send(output.read(cursor, max_bytes));
                    }
                }
            }
        });
        Self(sender)
    }

    async fn read(
        &self,
        cursor: Option<u64>,
        max_bytes: usize,
    ) -> Result<DebugOutputSlice, String> {
        let (reply, response) = oneshot::channel();
        self.0
            .send(StartupOutputCommand::Read {
                cursor,
                max_bytes,
                reply,
            })
            .await
            .map_err(|_| "DEV output owner stopped".to_owned())?;
        response
            .await
            .map_err(|_| "DEV output owner dropped its response".to_owned())
    }

    async fn head(&self) -> Result<u64, String> {
        let (reply, response) = oneshot::channel();
        self.0
            .send(StartupOutputCommand::Head(reply))
            .await
            .map_err(|_| "DEV output owner stopped".to_owned())?;
        response
            .await
            .map_err(|_| "DEV output owner dropped its cursor".to_owned())
    }
}

async fn capture_startup_output(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    output: StartupOutput,
) -> Result<(), String> {
    let mut chunk = [0_u8; 4096];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|error| format!("DEV output capture failed: {error}"))?;
        if count == 0 {
            return Ok(());
        }
        output
            .0
            .send(StartupOutputCommand::Append(chunk[..count].to_vec()))
            .await
            .map_err(|_| "DEV output owner stopped during capture".to_owned())?;
    }
}

async fn with_startup_diagnostics(message: String, output: &StartupOutput) -> String {
    let diagnostics = match output.read(None, STARTUP_LOG_TAIL_BYTES).await {
        Ok(tail) if tail.output.is_empty() => "startup output was empty".to_owned(),
        Ok(tail) => format!(
            "startup output (last {} bytes):\n{}",
            tail.output.len(),
            tail.output.trim_end()
        ),
        Err(error) => format!("startup output unavailable: {error}"),
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
