mod acp;
mod admin;
mod agent_defs_io;
mod agent_mcp_http;
mod agent_runtime;
mod backend;
mod backend_transport;
mod chat_buffer;
mod claude;
mod codex;
mod conversation;
mod debug_mcp_http;
mod dev_instance;
mod driver_mcp_http;
mod file_service;
mod file_watch;
mod gemini;
mod git_service;
pub mod host;
mod host_router;
mod kiro;
mod project_store;
mod protocol;
mod remote;
mod remote_control;
mod session_store;
mod skill_injection;
mod steering;
mod subprocess;
mod terminal;
mod tyde_server_conn;
mod usage;
mod workflow_io;

use parking_lot::Mutex as SyncMutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tokio::sync::{mpsc, watch, Mutex, Notify};

use crate::admin::AdminManager;
use crate::agent_runtime::{AgentEventBatch, AgentInfo, AgentRuntime, CollectedAgentResult};
use crate::backend::{
    BackendKind, BackendSession, SessionCommand, StartupMcpServer, StartupMcpTransport,
};
use crate::backend_transport::{BackendLaunchTarget, BackendTransport};
use crate::claude::{SubAgentEmitter, SubAgentHandle};
use crate::conversation::ConversationManager;
use crate::file_service::{FileContent, FileEntry};
use crate::file_watch::FileWatchManager;
use crate::git_service::GitFileStatus;
use crate::project_store::ProjectStore;
use crate::remote::{
    check_remote_tyde_install, compare_numeric_versions, connect_remote_with_progress,
    detect_remote_tyde_target, install_remote_tyde_binary, is_remote_tyde_server_running,
    launch_remote_tyde_headless, list_remote_tyde_installed_versions, parse_remote_path,
    parse_remote_workspace_roots, query_remote_tyde_server_version, resolve_remote_home_dir,
    resolve_remote_tyde_from_path, stop_remote_tyde_headless, to_remote_uri,
    tyde_socket_path_from_home, validate_remote_cli, SUBPROCESS_CRATE_NAME, SUBPROCESS_GIT_REPO,
    SUBPROCESS_VERSION,
};
use crate::session_store::SessionStore;
use crate::subprocess::ImageAttachment;
use crate::terminal::TerminalManager;

pub(crate) type AgentId = String;

/// Implements SubAgentEmitter for backend sessions that expose provider-native
/// sub-agent lifecycle events. Registers sub-agents in the Tyde AgentRuntime
/// and creates per-sub-agent event forwarding.
///
/// For non-bridge conversations, `parent_agent_id` starts as `None`. The first
/// time a sub-agent is spawned, the parent conversation is lazily registered in
/// the AgentRuntime so that the parent-child hierarchy is visible in the UI.
struct BackendSubAgentEmitter {
    app: tauri::AppHandle,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    parent_agent_id: Option<AgentId>,
    /// Lazily populated when `parent_agent_id` is `None` and the first
    /// sub-agent is spawned. Subsequent sub-agents reuse this value.
    lazy_parent_agent_id: Mutex<Option<AgentId>>,
    parent_conversation_id: u64,
    workspace_roots: Vec<String>,
    backend_kind: String,
    assistant_sender_name: String,
    session_store: Arc<SyncMutex<SessionStore>>,
    conversation_to_session: Arc<SyncMutex<HashMap<u64, String>>>,
}

impl BackendSubAgentEmitter {
    /// Resolve the parent agent_id. If no explicit parent was set (non-bridge
    /// conversations), lazily register the parent conversation in the runtime.
    async fn resolve_parent_agent_id(&self) -> Option<AgentId> {
        if let Some(id) = &self.parent_agent_id {
            return Some(id.clone());
        }
        let mut lazy = self.lazy_parent_agent_id.lock().await;
        if let Some(id) = lazy.as_ref() {
            return Some(id.clone());
        }
        let (id, created) = {
            // Reuse existing registration if present; otherwise register the
            // parent conversation so sub-agents can reference it.
            let mut runtime = self.agent_runtime.lock().await;
            if let Some(existing) = runtime.get_agent_by_conversation(self.parent_conversation_id) {
                (existing.agent_id, false)
            } else {
                (
                    runtime
                        .register_agent(
                            self.parent_conversation_id,
                            self.workspace_roots.clone(),
                            self.backend_kind.clone(),
                            None,
                            "Conversation".to_string(),
                            None,
                        )
                        .agent_id,
                    true,
                )
            }
        };
        *lazy = Some(id.clone());
        if created {
            self.agent_runtime_notify.notify_waiters();
            self.emit_agent_changed(&id).await;
        }
        Some(id)
    }

    async fn emit_agent_changed(&self, agent_id: &str) {
        let info = { self.agent_runtime.lock().await.get_agent(agent_id) };
        if let Some(info) = info {
            let _ = self.app.emit("agent-changed", &info);
        }
    }
}

impl SubAgentEmitter for BackendSubAgentEmitter {
    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubAgentHandle> + Send + '_>> {
        Box::pin(async move {
            let parent_agent_id = self.resolve_parent_agent_id().await;

            let (event_tx, event_rx) = mpsc::unbounded_channel();

            let conversation_id = {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                tool_use_id.hash(&mut hasher);
                self.parent_conversation_id
                    .wrapping_add(1_000_000)
                    .wrapping_add(hasher.finish() % 1_000_000)
            };

            let agent_info = {
                let mut runtime = self.agent_runtime.lock().await;
                let display_name = if name.is_empty() {
                    "Sub-agent".to_string()
                } else {
                    name
                };
                let mut info = runtime.register_agent(
                    conversation_id,
                    self.workspace_roots.clone(),
                    self.backend_kind.clone(),
                    parent_agent_id.clone(),
                    display_name,
                    None,
                );
                info.agent_type = if agent_type.is_empty() {
                    None
                } else {
                    Some(agent_type)
                };
                runtime.update_agent_type(&info.agent_id, info.agent_type.clone());
                info
            };
            self.agent_runtime_notify.notify_waiters();
            self.emit_agent_changed(&agent_info.agent_id).await;

            tracing::info!(
                "{} sub-agent spawned: agent_id={}, conversation_id={}, parent={:?}, tool_use_id={}",
                self.backend_kind,
                agent_info.agent_id,
                conversation_id,
                parent_agent_id,
                tool_use_id,
            );

            // Create session store record for this sub-agent
            {
                let workspace_root = self.workspace_roots.first().map(|s| s.as_str());
                let parent_tyde_id = self
                    .conversation_to_session
                    .lock()
                    .get(&self.parent_conversation_id)
                    .cloned();
                let mut store = self.session_store.lock();
                match store.create(&self.backend_kind, workspace_root) {
                    Ok(record) => {
                        let sub_sid = record.id;
                        if let Some(ref parent_id) = parent_tyde_id {
                            if let Err(err) = store.set_parent(&sub_sid, parent_id) {
                                tracing::error!("Failed to set sub-agent parent_id: {err}");
                            }
                        }
                        if !agent_info.name.is_empty() && !is_generic_agent_name(&agent_info.name) {
                            if let Err(err) = store.set_alias(&sub_sid, &agent_info.name) {
                                tracing::error!("Failed to set sub-agent alias: {err}");
                            }
                        }
                        drop(store);
                        self.conversation_to_session
                            .lock()
                            .insert(conversation_id, sub_sid);
                    }
                    Err(err) => {
                        tracing::error!(
                            "Failed to create session store record for sub-agent: {err}"
                        );
                    }
                }
            }

            // Forward sub-agent events to the frontend
            let app = self.app.clone();
            let runtime = Arc::clone(&self.agent_runtime);
            let notify = Arc::clone(&self.agent_runtime_notify);
            let registration = serde_json::json!({
                "kind": "ConversationRegistered",
                "data": {
                    "agent_id": &agent_info.agent_id,
                    "workspace_roots": self.workspace_roots,
                    "backend_kind": &self.backend_kind,
                    "name": &agent_info.name,
                    "agent_type": &agent_info.agent_type,
                    "parent_agent_id": parent_agent_id,
                    "ui_owner_project_id": &agent_info.ui_owner_project_id,
                }
            });
            let (settings_tx, _) = watch::channel(Value::Null);
            tokio::spawn(forward_events(
                app.clone(),
                conversation_id,
                event_rx,
                runtime,
                notify,
                registration,
                settings_tx,
                self.session_store.clone(),
                self.conversation_to_session.clone(),
            ));

            // Queue a synthetic user message with the parent task text when available.
            let initial_message = description.trim().to_string();
            if !initial_message.is_empty() {
                let _ = event_tx.send(serde_json::json!({
                    "kind": "MessageAdded",
                    "data": {
                        "timestamp": crate::claude::unix_now_ms(),
                        "content": initial_message,
                        "sender": "User",
                        "tool_calls": [],
                        "images": [],
                    }
                }));
            }
            let _ = event_tx.send(serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": true,
            }));

            SubAgentHandle {
                agent_id: agent_info.agent_id,
                conversation_id,
                event_tx,
            }
        })
    }

    fn on_subagent_completed(
        &self,
        tool_use_id: &str,
        agent_id: AgentId,
        success: bool,
        final_response: Option<String>,
        event_tx: mpsc::UnboundedSender<Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let tool_use_id = tool_use_id.to_string();
        Box::pin(async move {
            let final_response = final_response
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| text.to_string());

            let should_emit = {
                let runtime = self.agent_runtime.lock().await;
                let current_info = runtime.get_agent(&agent_id);
                let already_stopped = current_info
                    .as_ref()
                    .map(|info| !info.is_running)
                    .unwrap_or(false);
                let final_response_differs = match (
                    final_response.as_ref(),
                    current_info
                        .as_ref()
                        .and_then(|info| info.last_message.as_ref()),
                ) {
                    (Some(next), Some(existing)) => next != existing,
                    (Some(_), None) => true,
                    _ => false,
                };
                !already_stopped || final_response_differs
            };

            // Route terminal events through the sub-agent's event channel so
            // forward_events processes them in order — after any earlier queued
            // events like TypingStatusChanged(true).  This prevents the race
            // where forward_events processes a stale TypingStatusChanged(true)
            // after we've already recorded TypingStatusChanged(false).
            if should_emit {
                let summary = final_response.clone().unwrap_or_else(|| {
                    if success {
                        "Completed".to_string()
                    } else {
                        "Failed".to_string()
                    }
                });
                let terminal_event = serde_json::json!({
                    "kind": if success { "StreamEnd" } else { "Error" },
                    "data": if success {
                        serde_json::json!({
                            "message": {
                                "timestamp": crate::claude::unix_now_ms(),
                                "sender": { "Assistant": { "agent": &self.assistant_sender_name } },
                                "content": summary,
                                "tool_calls": [],
                                "images": [],
                            }
                        })
                    } else {
                        serde_json::json!(summary)
                    }
                });
                let _ = event_tx.send(terminal_event);
            }

            let _ = event_tx.send(serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": false,
            }));

            tracing::info!(
                "{} sub-agent completed: agent_id={}, tool_use_id={}, success={}",
                self.backend_kind,
                agent_id,
                tool_use_id,
                success,
            );
        })
    }
}

fn backend_assistant_sender_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Codex => "Codex",
        BackendKind::Claude => "Claude",
        BackendKind::Kiro => "Kiro",
        BackendKind::Gemini => "Gemini",
    }
}

pub(crate) struct AppState {
    manager: Mutex<ConversationManager>,
    admin: Mutex<AdminManager>,
    terminals: Mutex<TerminalManager>,
    file_watch: SyncMutex<Option<FileWatchManager>>,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    session_store: Arc<SyncMutex<SessionStore>>,
    project_store: Arc<SyncMutex<ProjectStore>>,
    host_store: SyncMutex<host::HostStore>,
    conversation_to_session: Arc<SyncMutex<HashMap<u64, String>>>,
    /// Senders for forwarding remote TydeServer chat events through
    /// the unified `forward_events` pipeline.
    remote_chat_senders: Arc<SyncMutex<HashMap<u64, mpsc::UnboundedSender<Value>>>>,
    mcp_http_enabled: SyncMutex<bool>,
    mcp_control_enabled: SyncMutex<bool>,
    driver_mcp_http_enabled: SyncMutex<bool>,
    driver_mcp_http_autoload: SyncMutex<bool>,
    driver_mcp_http_env_override: bool,
    debug_event_log: SyncMutex<DebugEventLog>,
    debug_ui_pending:
        SyncMutex<HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>>>,
    debug_ui_request_seq: AtomicU64,
    remote_control_enabled: SyncMutex<bool>,
    disabled_backends: SyncMutex<HashSet<String>>,
    settings_watch: Mutex<HashMap<u64, watch::Sender<Value>>>,
    dev_instances: SyncMutex<dev_instance::DevInstanceRegistry>,
    tyde_server_connections:
        SyncMutex<HashMap<String, Arc<tyde_server_conn::TydeServerConnection>>>,
    /// Cleanup handles for skills injected per-conversation.
    skill_cleanups: SyncMutex<HashMap<u64, skill_injection::SkillCleanup>>,
}

#[derive(Serialize, Clone)]
struct ChatEventPayload {
    conversation_id: u64,
    event: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppSettings {
    #[serde(default = "default_mcp_http_enabled")]
    mcp_http_enabled: bool,
    #[serde(default = "default_mcp_control_enabled")]
    mcp_control_enabled: bool,
    #[serde(default = "default_driver_mcp_http_enabled")]
    driver_mcp_http_enabled: bool,
    #[serde(default = "default_driver_mcp_http_autoload")]
    driver_mcp_http_autoload: bool,
    #[serde(default)]
    remote_control_enabled: bool,
    // Legacy field — kept for serde backward compat, no longer read.
    #[serde(default)]
    default_backend: String,
}

fn default_mcp_http_enabled() -> bool {
    true
}

fn default_mcp_control_enabled() -> bool {
    true
}

fn default_driver_mcp_http_enabled() -> bool {
    false
}

fn default_driver_mcp_http_autoload() -> bool {
    false
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            mcp_http_enabled: default_mcp_http_enabled(),
            mcp_control_enabled: default_mcp_control_enabled(),
            driver_mcp_http_enabled: default_driver_mcp_http_enabled(),
            driver_mcp_http_autoload: default_driver_mcp_http_autoload(),
            remote_control_enabled: false,
            default_backend: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct McpHttpServerSettings {
    enabled: bool,
    running: bool,
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DriverMcpHttpServerSettings {
    enabled: bool,
    autoload: bool,
    running: bool,
    url: Option<String>,
}

const DEFAULT_DEBUG_EVENT_LOG_LIMIT: usize = 10_000;
const DEFAULT_DEBUG_EVENTS_LIMIT: usize = 200;
const MAX_DEBUG_EVENTS_LIMIT: usize = 2_000;
const DEFAULT_DEBUG_UI_TIMEOUT_MS: u64 = 5_000;
const MAX_DEBUG_UI_TIMEOUT_MS: u64 = 60_000;
const MAX_DEBUG_EVENT_SUMMARY_LEN: usize = 512;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DebugEventEntry {
    seq: u64,
    stream: String,
    timestamp_ms: u64,
    payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DebugEventBatch {
    events: Vec<DebugEventEntry>,
    latest_seq: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DebugEventsSinceRequest {
    pub(crate) since_seq: Option<u64>,
    pub(crate) limit: Option<usize>,
    pub(crate) stream: Option<String>,
}

#[derive(Debug)]
struct DebugEventLog {
    next_seq: u64,
    events: VecDeque<DebugEventEntry>,
    limit: usize,
}

impl DebugEventLog {
    fn new() -> Self {
        Self {
            next_seq: 1,
            events: VecDeque::new(),
            limit: DEFAULT_DEBUG_EVENT_LOG_LIMIT,
        }
    }

    fn push(&mut self, stream: &str, payload: Value) {
        let event = DebugEventEntry {
            seq: self.next_seq,
            stream: stream.to_string(),
            timestamp_ms: now_ms(),
            payload,
        };
        self.next_seq += 1;
        self.events.push_back(event);
        while self.events.len() > self.limit {
            let _ = self.events.pop_front();
        }
    }

    fn events_since(&self, since_seq: u64, limit: usize, stream: Option<&str>) -> DebugEventBatch {
        let normalized_stream = stream.map(|raw| raw.trim()).filter(|raw| !raw.is_empty());
        let events = self
            .events
            .iter()
            .filter(|event| event.seq > since_seq)
            .filter(|event| {
                normalized_stream
                    .map(|value| event.stream == value)
                    .unwrap_or(true)
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        DebugEventBatch {
            events,
            latest_seq: self.next_seq.saturating_sub(1),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DebugConversationSnapshot {
    conversation_id: u64,
    backend_kind: Option<String>,
    workspace_roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DebugSnapshot {
    timestamp_ms: u64,
    conversations: Vec<DebugConversationSnapshot>,
    admin_subprocess_ids: Vec<u64>,
    terminal_ids: Vec<u64>,
    runtime_agents: Vec<AgentInfo>,
    agent_mcp_http: McpHttpServerSettings,
    driver_mcp_http: DriverMcpHttpServerSettings,
}

#[derive(Debug, Clone, Serialize)]
struct DebugUiRequestPayload {
    request_id: String,
    action: String,
    params: Value,
}

#[derive(Debug, Clone, Serialize)]
struct CreateWorkbenchEventPayload {
    parent_workspace_path: String,
    branch: String,
    worktree_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeleteWorkbenchEventPayload {
    workspace_path: String,
}

#[derive(Serialize, Clone)]
struct CreateConversationResponse {
    conversation_id: u64,
    session_id: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SpawnAgentResponse {
    pub(crate) agent_id: AgentId,
    pub(crate) conversation_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SpawnAgentRequest {
    pub(crate) workspace_roots: Vec<String>,
    pub(crate) prompt: String,
    pub(crate) backend_kind: Option<String>,
    pub(crate) parent_agent_id: Option<AgentId>,
    pub(crate) ui_owner_project_id: Option<String>,
    pub(crate) name: String,
    pub(crate) ephemeral: Option<bool>,
    /// Images to attach to the initial message sent to the agent.
    #[serde(skip)]
    pub(crate) images: Option<Vec<ImageAttachment>>,
    pub(crate) agent_definition_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SendAgentMessageRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgentIdRequest {
    pub(crate) agent_id: AgentId,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WaitForAgentRequest {
    pub(crate) agent_id: AgentId,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgentEventsSinceRequest {
    pub(crate) since_seq: Option<u64>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AwaitAgentsRequest {
    pub(crate) agent_ids: Vec<AgentId>,
    pub(crate) timeout_ms: Option<u64>,
}

/// Simplified agent result returned by the push-oriented MCP tools.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct AgentResult {
    pub(crate) agent_id: AgentId,
    pub(crate) is_running: bool,
    pub(crate) message: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) summary: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct AwaitAgentsResponse {
    pub(crate) ready: Vec<AgentResult>,
    pub(crate) still_running: Vec<AgentId>,
}

fn is_generic_agent_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "conversation"
        || lower == "bridge"
        || lower == "sub-agent"
        || lower.starts_with("agent ")
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

fn subprocess_path() -> Result<String, String> {
    if let Ok(path) = std::env::var("TYDE_SUBPROCESS_PATH") {
        tracing::info!("Found subprocess via TYDE_SUBPROCESS_PATH env var");
        return Ok(path);
    }
    tracing::warn!("TYDE_SUBPROCESS_PATH env var not set");

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join("tycode-subprocess");
            if is_executable(&sibling) {
                tracing::info!("Found subprocess as sibling of current executable");
                return Ok(sibling.to_string_lossy().to_string());
            }
        }
    }
    tracing::warn!("Subprocess not found as sibling of current executable");

    // Check on-demand install location: ~/.tycode/v{VERSION}/bin/tycode-subprocess
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let installed = PathBuf::from(&home).join(format!(
            ".tycode/v{}/bin/{}",
            SUBPROCESS_VERSION, SUBPROCESS_CRATE_NAME
        ));
        if is_executable(&installed) {
            tracing::info!("Found subprocess in on-demand install location");
            return Ok(installed.to_string_lossy().to_string());
        }
    }
    tracing::warn!("Subprocess not found in on-demand install location");

    // `cargo tauri dev` runs from the source root, not target/debug/,
    // so walk up from cwd looking for a cargo workspace's target directory.
    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let cargo_toml = dir.join("Cargo.toml");
            let is_workspace = fs::read_to_string(&cargo_toml)
                .map(|contents| contents.contains("[workspace]"))
                .unwrap_or(false);

            if is_workspace {
                let debug = dir.join("target/debug/tycode-subprocess");
                if is_executable(&debug) {
                    tracing::info!("Found subprocess in workspace target/debug");
                    return Ok(debug.to_string_lossy().to_string());
                }
                let release = dir.join("target/release/tycode-subprocess");
                if is_executable(&release) {
                    tracing::info!("Found subprocess in workspace target/release");
                    return Ok(release.to_string_lossy().to_string());
                }
            }

            if !dir.pop() {
                break;
            }
        }
    }
    tracing::warn!("Subprocess not found in any parent workspace target directory");

    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    if let Ok(output) = Command::new(which_cmd).arg("tycode-subprocess").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                tracing::info!("Found subprocess on system PATH");
                return Ok(path);
            }
        }
    }
    tracing::warn!("Subprocess not found on system PATH");

    Err("Could not find tycode-subprocess binary. \
         Set TYDE_SUBPROCESS_PATH env var or build it with: \
         cargo build -p tycode-subprocess"
        .to_string())
}

fn resolve_tyde_app_settings_path() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".tyde").join("app-settings.json"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(profile)
            .join(".tyde")
            .join("app-settings.json"));
    }
    Err("Could not determine home directory for app settings".to_string())
}

/// Read settings from disk without applying any env var overrides.
fn load_app_settings_from_disk() -> AppSettings {
    let path = match resolve_tyde_app_settings_path() {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!("Failed to resolve app settings path: {err}");
            return AppSettings::default();
        }
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::error!("Failed to read app settings from {}: {err}", path.display());
            }
            return AppSettings::default();
        }
    };

    match serde_json::from_str::<AppSettings>(&raw) {
        Ok(settings) => settings,
        Err(err) => {
            tracing::error!(
                "Failed to parse app settings from {}: {err}",
                path.display()
            );
            AppSettings::default()
        }
    }
}

fn load_app_settings() -> AppSettings {
    let mut settings = load_app_settings_from_disk();

    // Allow env vars to override settings (used by dev instances spawned from the host).
    if let Ok(val) = std::env::var("TYDE_DRIVER_MCP_HTTP_ENABLED") {
        let enabled = val == "true" || val == "1";
        settings.driver_mcp_http_enabled = enabled;
        settings.driver_mcp_http_autoload = enabled;
    }

    settings
}

fn save_app_settings(settings: &AppSettings) -> Result<(), String> {
    let path = resolve_tyde_app_settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create settings directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let data = serde_json::to_string_pretty(settings)
        .map_err(|err| format!("Failed to serialize app settings: {err}"))?;
    fs::write(&path, data)
        .map_err(|err| format!("Failed to write app settings to {}: {err}", path.display()))
}

fn app_settings_from_state(state: &AppState) -> AppSettings {
    // When driver settings were overridden by env var (dev instances), read the
    // driver fields from the on-disk file so we never clobber the host's saved values.
    let (driver_enabled, driver_autoload) = if state.driver_mcp_http_env_override {
        let on_disk = load_app_settings_from_disk();
        (
            on_disk.driver_mcp_http_enabled,
            on_disk.driver_mcp_http_autoload,
        )
    } else {
        (
            *state.driver_mcp_http_enabled.lock(),
            *state.driver_mcp_http_autoload.lock(),
        )
    };
    AppSettings {
        mcp_http_enabled: *state.mcp_http_enabled.lock(),
        mcp_control_enabled: *state.mcp_control_enabled.lock(),
        driver_mcp_http_enabled: driver_enabled,
        driver_mcp_http_autoload: driver_autoload,
        remote_control_enabled: *state.remote_control_enabled.lock(),
        default_backend: String::new(),
    }
}

fn current_mcp_http_server_settings(enabled: bool) -> McpHttpServerSettings {
    McpHttpServerSettings {
        enabled,
        running: agent_mcp_http::is_agent_mcp_http_server_running(),
        url: agent_mcp_http::agent_mcp_http_server_url(),
    }
}

fn current_driver_mcp_http_server_settings(
    enabled: bool,
    autoload: bool,
) -> DriverMcpHttpServerSettings {
    DriverMcpHttpServerSettings {
        enabled,
        autoload,
        running: driver_mcp_http::is_driver_mcp_http_server_running(),
        url: driver_mcp_http::driver_mcp_http_server_url(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

fn truncate_for_debug(text: &str, max_chars: usize) -> String {
    let truncated: String = text.chars().take(max_chars).collect();
    if truncated.chars().count() < text.chars().count() {
        format!("{truncated}… ({} chars)", text.chars().count())
    } else {
        truncated
    }
}

fn summarize_value_for_debug(value: &Value) -> Value {
    match value {
        Value::String(text) => {
            if text.chars().count() > MAX_DEBUG_EVENT_SUMMARY_LEN {
                Value::String(truncate_for_debug(text, MAX_DEBUG_EVENT_SUMMARY_LEN))
            } else {
                Value::String(text.clone())
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(summarize_value_for_debug)
                .collect::<Vec<_>>(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map.iter().take(30) {
                if key == "data" {
                    if let Some(text) = value.as_str() {
                        out.insert(
                            key.clone(),
                            Value::String(format!("(omitted {} chars)", text.len())),
                        );
                        continue;
                    }
                }
                out.insert(key.clone(), summarize_value_for_debug(value));
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn startup_mcp_servers_for_new_sessions(
    state: &AppState,
    include_agent_control: bool,
    workspace_roots: &[String],
) -> Result<Vec<StartupMcpServer>, String> {
    startup_mcp_servers_for_agent(state, include_agent_control, None, workspace_roots, &[])
}

fn startup_mcp_servers_for_agent(
    state: &AppState,
    include_agent_control: bool,
    caller_agent_id: Option<&str>,
    _workspace_roots: &[String],
    extra_mcp_servers: &[StartupMcpServer],
) -> Result<Vec<StartupMcpServer>, String> {
    let mut servers = Vec::new();

    if include_agent_control {
        let server_enabled = *state.mcp_http_enabled.lock();
        if !server_enabled {
            return Err("Tyde MCP control server must be enabled for agent control".to_string());
        }
        let control_enabled = *state.mcp_control_enabled.lock();
        if !control_enabled {
            return Err("MCP control injection is disabled".to_string());
        }

        let Some(url) = agent_mcp_http::agent_mcp_http_server_url() else {
            return Err("Tyde MCP control server is not running".to_string());
        };
        if url.trim().is_empty() {
            return Err("Tyde MCP control server URL is unavailable".to_string());
        }

        let mut headers = HashMap::new();
        if let Some(agent_id) = caller_agent_id {
            headers.insert("X-Tyde-Agent-Id".to_string(), agent_id.to_string());
        }

        servers.push(StartupMcpServer {
            name: "tyde_agent_control".to_string(),
            transport: StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var: None,
            },
        });
    }

    let driver_enabled = *state.driver_mcp_http_enabled.lock();
    let driver_autoload = *state.driver_mcp_http_autoload.lock();
    if driver_enabled && driver_autoload {
        if let Some(url) = driver_mcp_http::driver_mcp_http_server_url() {
            if !url.trim().is_empty() {
                servers.push(StartupMcpServer {
                    name: "tyde_driver".to_string(),
                    transport: StartupMcpTransport::Http {
                        url,
                        headers: HashMap::new(),
                        bearer_token_env_var: None,
                    },
                });
            }
        }
    }

    servers.extend(extra_mcp_servers.iter().cloned());

    Ok(servers)
}

/// Convert agent definition MCP servers to startup format.
fn definition_mcp_servers_to_startup(
    servers: &[agent_defs_io::AgentMcpServer],
) -> Vec<StartupMcpServer> {
    servers
        .iter()
        .map(|s| StartupMcpServer {
            name: s.name.clone(),
            transport: match &s.transport {
                agent_defs_io::AgentMcpTransport::Http { url, headers } => {
                    StartupMcpTransport::Http {
                        url: url.clone(),
                        headers: headers.clone(),
                        bearer_token_env_var: None,
                    }
                }
                agent_defs_io::AgentMcpTransport::Stdio { command, args, env } => {
                    StartupMcpTransport::Stdio {
                        command: command.clone(),
                        args: args.clone(),
                        env: env.clone(),
                    }
                }
            },
        })
        .collect()
}

/// Compose steering content from definition instructions, tool policy, skills, and workspace steering.
fn compose_steering(
    instructions: Option<&str>,
    tool_policy: &agent_defs_io::ToolPolicy,
    workspace_steering: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(instr) = instructions {
        if !instr.trim().is_empty() {
            parts.push(instr.to_string());
        }
    }

    match tool_policy {
        agent_defs_io::ToolPolicy::AllowList(tools) if !tools.is_empty() => {
            parts.push(format!(
                "[Tool Policy]\nYou MUST only use the following tools: {}",
                tools.join(", ")
            ));
        }
        agent_defs_io::ToolPolicy::DenyList(tools) if !tools.is_empty() => {
            parts.push(format!(
                "[Tool Policy]\nYou MUST NOT use the following tools: {}",
                tools.join(", ")
            ));
        }
        _ => {}
    }

    if let Some(ws) = workspace_steering {
        if !ws.trim().is_empty() {
            parts.push(ws.to_string());
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn record_debug_event(state: &AppState, stream: &str, payload: Value) {
    state.debug_event_log.lock().push(stream, payload);
}

pub(crate) fn record_debug_event_from_app(app: &tauri::AppHandle, stream: &str, payload: Value) {
    let state = app.state::<AppState>();
    record_debug_event(state.inner(), stream, payload);
}

fn normalize_debug_events_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(DEFAULT_DEBUG_EVENTS_LIMIT)
        .clamp(1, MAX_DEBUG_EVENTS_LIMIT)
}

fn normalize_debug_ui_timeout_ms(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_DEBUG_UI_TIMEOUT_MS)
        .clamp(1, MAX_DEBUG_UI_TIMEOUT_MS)
}

pub(crate) async fn debug_events_since_internal(
    state: &AppState,
    request: DebugEventsSinceRequest,
) -> Result<DebugEventBatch, String> {
    let since_seq = request.since_seq.unwrap_or(0);
    let limit = normalize_debug_events_limit(request.limit);
    let stream = request
        .stream
        .as_ref()
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty());
    let log = state.debug_event_log.lock();
    Ok(log.events_since(since_seq, limit, stream))
}

pub(crate) async fn debug_snapshot_internal(state: &AppState) -> Result<DebugSnapshot, String> {
    let conversations = {
        let mgr = state.manager.lock().await;
        let mut ids = mgr.active_ids();
        ids.sort_unstable();
        ids.into_iter()
            .map(|conversation_id| DebugConversationSnapshot {
                conversation_id,
                backend_kind: mgr
                    .backend_kind(conversation_id)
                    .map(|kind| kind.to_string()),
                workspace_roots: mgr
                    .workspace_roots(conversation_id)
                    .map(|roots| roots.to_vec())
                    .unwrap_or_else(|| {
                        tracing::warn!("No workspace roots for conversation {conversation_id}");
                        Vec::new()
                    }),
            })
            .collect::<Vec<_>>()
    };

    let admin_subprocess_ids = {
        let mgr = state.admin.lock().await;
        let mut ids = mgr.active_ids();
        ids.sort_unstable();
        ids
    };

    let terminal_ids = {
        let mgr = state.terminals.lock().await;
        let mut ids = mgr.list_ids();
        ids.sort_unstable();
        ids
    };

    let runtime_agents = {
        let runtime = state.agent_runtime.lock().await;
        runtime.list_agents()
    };

    let agent_enabled = *state.mcp_http_enabled.lock();
    let driver_enabled = *state.driver_mcp_http_enabled.lock();
    let driver_autoload = *state.driver_mcp_http_autoload.lock();

    Ok(DebugSnapshot {
        timestamp_ms: now_ms(),
        conversations,
        admin_subprocess_ids,
        terminal_ids,
        runtime_agents,
        agent_mcp_http: current_mcp_http_server_settings(agent_enabled),
        driver_mcp_http: current_driver_mcp_http_server_settings(driver_enabled, driver_autoload),
    })
}

pub(crate) async fn debug_ui_action_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    action: &str,
    params: Value,
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    let request_id = format!(
        "dbg-ui-{}",
        state.debug_ui_request_seq.fetch_add(1, Ordering::Relaxed)
    );

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value, String>>();
    {
        let mut pending = state.debug_ui_pending.lock();
        pending.insert(request_id.clone(), tx);
    }

    let payload = DebugUiRequestPayload {
        request_id: request_id.clone(),
        action: action.to_string(),
        params: params.clone(),
    };
    record_debug_event(
        state,
        "ui_request",
        serde_json::json!({
            "request_id": request_id,
            "action": action,
            "params": summarize_value_for_debug(&params),
        }),
    );
    if let Err(err) = app.emit("tyde-debug-ui-request", &payload) {
        state.debug_ui_pending.lock().remove(&payload.request_id);
        return Err(format!("Failed to emit debug UI request: {err:?}"));
    }

    let timeout = normalize_debug_ui_timeout_ms(timeout_ms);
    match tokio::time::timeout(Duration::from_millis(timeout), rx).await {
        Ok(Ok(Ok(result))) => Ok(result),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(_)) => Err("Debug UI response channel closed".to_string()),
        Err(_) => {
            state.debug_ui_pending.lock().remove(&payload.request_id);
            Err(format!(
                "Debug UI action '{action}' timed out after {timeout}ms"
            ))
        }
    }
}

pub(crate) async fn create_workbench_internal(
    app: &tauri::AppHandle,
    parent_workspace_path: String,
    branch: String,
) -> Result<String, String> {
    let worktree_path = format!("{parent_workspace_path}--{branch}");
    git_service::git_worktree_add(&parent_workspace_path, &worktree_path, &branch).await?;

    // Register the new workbench in the project store.
    // If the parent workspace isn't tracked yet, add it first.
    let state = app.state::<AppState>();
    let mut store = state.project_store.lock();
    let parent_id = match store
        .get_by_workspace_path(&parent_workspace_path)
        .map(|r| r.id.clone())
    {
        Some(id) => id,
        None => {
            let parent_name = parent_workspace_path
                .rsplit('/')
                .next()
                .unwrap_or(&parent_workspace_path);
            match store.add(&parent_workspace_path, parent_name) {
                Ok(record) => record.id,
                Err(err) => {
                    tracing::warn!("Failed to auto-add parent project for workbench: {err}");
                    drop(store);
                    emit_projects_changed(app, &state);
                    return Ok(worktree_path);
                }
            }
        }
    };
    if let Err(err) = store.add_workbench(&parent_id, &worktree_path, &branch, "git-worktree") {
        tracing::warn!("Failed to register workbench in project store: {err}");
    }
    drop(store);
    emit_projects_changed(app, &state);

    app.emit(
        "tyde-create-workbench",
        &CreateWorkbenchEventPayload {
            parent_workspace_path,
            branch,
            worktree_path: worktree_path.clone(),
        },
    )
    .map_err(|err| format!("Failed to emit create workbench event: {err:?}"))?;
    Ok(worktree_path)
}

pub(crate) async fn delete_workbench_internal(
    app: &tauri::AppHandle,
    workspace_path: String,
) -> Result<(), String> {
    // Remove the workbench from the project store
    let state = app.state::<AppState>();
    let mut store = state.project_store.lock();
    let project_id = store
        .get_by_workspace_path(&workspace_path)
        .map(|r| r.id.clone());
    if let Some(project_id) = project_id {
        if let Err(err) = store.remove(&project_id) {
            tracing::warn!("Failed to remove workbench from project store: {err}");
        }
    }
    drop(store);
    emit_projects_changed(app, &state);

    app.emit(
        "tyde-delete-workbench",
        &DeleteWorkbenchEventPayload { workspace_path },
    )
    .map_err(|err| format!("Failed to emit delete workbench event: {err:?}"))?;
    Ok(())
}

pub(crate) fn resolve_requested_backend_kind(
    state: &AppState,
    backend_kind: Option<String>,
    workspace_roots: &[String],
) -> Result<BackendKind, String> {
    let host = host_router::resolve_host_for_roots(state, workspace_roots)?;

    let kind = match backend_kind {
        Some(raw) if !raw.trim().is_empty() => raw.parse::<BackendKind>()?,
        _ => host
            .default_backend
            .parse::<BackendKind>()
            .unwrap_or(BackendKind::Tycode),
    };

    if !host.enabled_backends.iter().any(|b| b == kind.as_str()) {
        return Err(format!(
            "Backend '{}' is not enabled for host '{}'",
            kind.as_str(),
            host.label
        ));
    }

    Ok(kind)
}

pub(crate) async fn materialize_remote_conversation(
    app: &tauri::AppHandle,
    state: &AppState,
    conn: Arc<tyde_server_conn::TydeServerConnection>,
    server_cid: u64,
    backend_kind_str: &str,
    workspace_roots: &[String],
) -> Result<u64, String> {
    if let Some(existing) = conn.translate_conversation_id(server_cid).await {
        let exists = {
            let mgr = state.manager.lock().await;
            mgr.get(existing).is_some()
        };
        if exists {
            ensure_settings_watch_sender(state, existing).await;
            return Ok(existing);
        }
    }

    let backend_kind = backend_kind_str.parse::<BackendKind>().map_err(|err| {
        format!(
            "Unknown backend kind '{}' for remote conversation {}: {}",
            backend_kind_str, server_cid, err
        )
    })?;

    let normalized_workspace_roots: Vec<String> = workspace_roots
        .iter()
        .map(|root| {
            if parse_remote_path(root).is_some() {
                root.clone()
            } else if root.starts_with('/') {
                to_remote_uri(conn.ssh_host(), root)
            } else {
                root.clone()
            }
        })
        .collect();

    let proxy = backend::TydeServerProxySession {
        connection: conn.clone(),
        server_conversation_id: server_cid,
        backend_kind,
    };

    let local_id = {
        let mut mgr = state.manager.lock().await;
        mgr.create_conversation(
            BackendSession::TydeServer(proxy),
            &normalized_workspace_roots,
        )
    };

    conn.register_conversation_mapping(server_cid, local_id)
        .await;

    // Session tracking is server-authoritative — no local SessionRecord.
    // The server's records are synced via handshake + list_session_records.

    // Spawn forward_events for chat event forwarding to the frontend.
    // No conversation_to_session mapping, so store updates are skipped.
    let (tx, rx) = mpsc::unbounded_channel();
    state.remote_chat_senders.lock().insert(local_id, tx);

    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(local_id, settings_tx.clone());
    }

    tokio::spawn(forward_events(
        app.clone(),
        local_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        Value::Null, // remote — server sends ConversationRegistered via replay
        settings_tx,
        state.session_store.clone(),
        state.conversation_to_session.clone(),
    ));

    Ok(local_id)
}

async fn ensure_settings_watch_sender(state: &AppState, conversation_id: u64) {
    let mut watchers = state.settings_watch.lock().await;
    if watchers.contains_key(&conversation_id) {
        return;
    }
    let (settings_tx, _) = watch::channel(Value::Null);
    watchers.insert(conversation_id, settings_tx);
}

async fn ensure_conversation_agent_registered(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
    workspace_roots: &[String],
    backend_kind: &str,
    name: &str,
    ui_owner_project_id: Option<String>,
) -> AgentInfo {
    if let Some(existing) = {
        let runtime = state.agent_runtime.lock().await;
        runtime.get_agent_by_conversation(conversation_id)
    } {
        return existing;
    }

    let info = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.register_agent(
            conversation_id,
            workspace_roots.to_vec(),
            backend_kind.to_string(),
            None,
            name.to_string(),
            ui_owner_project_id,
        )
    };
    state.agent_runtime_notify.notify_waiters();
    emit_agent_changed(app, state, &info.agent_id).await;
    info
}

async fn create_conversation_via_server(
    app: &tauri::AppHandle,
    state: &AppState,
    conn: Arc<tyde_server_conn::TydeServerConnection>,
    workspace_roots: Vec<String>,
    backend_kind: Option<String>,
    ephemeral: Option<bool>,
    agent_definition_id: Option<String>,
    ui_owner_project_id: Option<String>,
) -> Result<CreateConversationResponse, String> {
    let remote_roots = host_router::strip_ssh_roots(&workspace_roots);

    let resp = conn
        .invoke(
            "create_conversation",
            serde_json::json!({
                "workspace_roots": remote_roots,
                "backend_kind": backend_kind,
                "ephemeral": ephemeral,
                "agent_definition_id": agent_definition_id,
                "ui_owner_project_id": ui_owner_project_id,
            }),
        )
        .await?;

    let server_conv_id = resp
        .get("conversation_id")
        .and_then(|v| v.as_u64())
        .ok_or("Server did not return conversation_id")?;
    let session_id = resp
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let backend_kind_str = if let Some(kind) = resp.get("backend_kind").and_then(|v| v.as_str()) {
        kind.to_string()
    } else {
        tracing::warn!(
            "Remote create_conversation response missing backend_kind; using local fallback"
        );
        resolve_requested_backend_kind(state, backend_kind.clone(), &workspace_roots)?
            .as_str()
            .to_string()
    };
    let local_id = materialize_remote_conversation(
        app,
        state,
        conn.clone(),
        server_conv_id,
        &backend_kind_str,
        &workspace_roots,
    )
    .await?;

    // Sync the cached remote_session_records so that owns_session_record()
    // will recognise this newly-created session (needed for rename, delete, etc.)
    conn.fetch_session_records().await.map_err(|err| {
        format!("Created conversation but failed to refresh remote session cache: {err}")
    })?;

    Ok(CreateConversationResponse {
        conversation_id: local_id,
        session_id,
    })
}

async fn spawn_agent_via_server(
    app: &tauri::AppHandle,
    state: &AppState,
    conn: Arc<tyde_server_conn::TydeServerConnection>,
    request: SpawnAgentRequest,
) -> Result<SpawnAgentResponse, String> {
    let requested_backend_kind = request.backend_kind.clone();
    let remote_roots = host_router::strip_ssh_roots(&request.workspace_roots);

    let resp = conn
        .invoke(
            "spawn_agent",
            serde_json::json!({
                "workspace_roots": remote_roots,
                "prompt": request.prompt,
                "backend_kind": request.backend_kind,
                "parent_agent_id": request.parent_agent_id,
                "ui_owner_project_id": request.ui_owner_project_id,
                "name": request.name,
                "ephemeral": request.ephemeral,
                "agent_definition_id": request.agent_definition_id,
            }),
        )
        .await?;

    let agent_id = resp
        .get("agent_id")
        .and_then(|v| v.as_str())
        .ok_or("Server did not return agent_id")?
        .to_string();
    let conversation_id = resp
        .get("conversation_id")
        .and_then(|v| v.as_u64())
        .ok_or("Server did not return conversation_id")?;
    conn.register_remote_agent_id(agent_id.clone()).await;
    let backend_kind_str = if let Some(kind) = resp.get("backend_kind").and_then(|v| v.as_str()) {
        kind.to_string()
    } else {
        tracing::warn!("Remote spawn_agent response missing backend_kind; using local fallback");
        resolve_requested_backend_kind(state, requested_backend_kind, &request.workspace_roots)?
            .as_str()
            .to_string()
    };
    let local_conversation_id = materialize_remote_conversation(
        app,
        state,
        conn.clone(),
        conversation_id,
        &backend_kind_str,
        &request.workspace_roots,
    )
    .await?;

    // Sync the cached remote_session_records so that owns_session_record()
    // will recognise this newly-created session (needed for rename, delete, etc.)
    conn.fetch_session_records().await.map_err(|err| {
        format!("Spawned agent but failed to refresh remote session cache: {err}")
    })?;

    Ok(SpawnAgentResponse {
        agent_id,
        conversation_id: local_conversation_id,
    })
}

pub(crate) async fn resolve_backend_launch_target(
    app: &tauri::AppHandle,
    workspace_roots: &[String],
    backend_kind: BackendKind,
) -> Result<BackendLaunchTarget, String> {
    match backend_kind {
        BackendKind::Tycode => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    let path = connect_remote_with_progress(app, host).await?;
                    launch_target_for_backend(backend_kind, Some(host.clone()), Some(path))
                }
                None => launch_target_for_backend(backend_kind, None, Some(subprocess_path()?)),
            }
        }
        BackendKind::Codex => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "codex").await?;
                    launch_target_for_backend(backend_kind, Some(host.clone()), None)
                }
                None => launch_target_for_backend(backend_kind, None, None),
            }
        }
        BackendKind::Claude => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "claude").await?;
                    launch_target_for_backend(backend_kind, Some(host.clone()), None)
                }
                None => launch_target_for_backend(backend_kind, None, None),
            }
        }
        BackendKind::Kiro => {
            // parse_remote_workspace_roots errors on mixed local+SSH roots.
            // For admin sessions with mixed bridge projects, fall through to
            // local so list_sessions can query both local and SSH roots.
            match parse_remote_workspace_roots(workspace_roots) {
                Ok(Some((host, _))) => {
                    validate_remote_cli(app, &host, "kiro-cli-chat").await?;
                    launch_target_for_backend(backend_kind, Some(host), None)
                }
                _ => launch_target_for_backend(backend_kind, None, None),
            }
        }
        BackendKind::Gemini => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "gemini").await?;
                    launch_target_for_backend(backend_kind, Some(host.clone()), None)
                }
                None => launch_target_for_backend(backend_kind, None, None),
            }
        }
    }
}

fn launch_target_for_backend(
    backend_kind: BackendKind,
    remote_host: Option<String>,
    executable_path: Option<String>,
) -> Result<BackendLaunchTarget, String> {
    match backend_kind {
        BackendKind::Tycode => {
            let path = executable_path
                .ok_or("Missing tycode executable path for launch target".to_string())?;
            match remote_host {
                Some(host) => Ok(BackendLaunchTarget::remote(host, path)),
                None => Ok(BackendLaunchTarget::local(path)),
            }
        }
        BackendKind::Codex | BackendKind::Claude | BackendKind::Kiro | BackendKind::Gemini => {
            match remote_host {
                Some(host) => Ok(BackendLaunchTarget::remote(host, String::new())),
                None => Ok(BackendLaunchTarget::local(String::new())),
            }
        }
    }
}

#[derive(Serialize)]
struct BackendDepResult {
    available: bool,
    binary_name: String,
}

#[derive(Serialize)]
struct BackendDependencyStatus {
    tycode: BackendDepResult,
    codex: BackendDepResult,
    claude: BackendDepResult,
    kiro: BackendDepResult,
    gemini: BackendDepResult,
}

#[tauri::command]
fn get_initial_workspace() -> Option<String> {
    std::env::var("TYDE_OPEN_WORKSPACE").ok()
}

#[tauri::command]
fn check_backend_dependencies() -> BackendDependencyStatus {
    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };

    let check = |binary: &str| -> BackendDepResult {
        let available = Command::new(which_cmd)
            .arg(binary)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        BackendDepResult {
            available,
            binary_name: binary.to_string(),
        }
    };

    BackendDependencyStatus {
        tycode: BackendDepResult {
            available: subprocess_path().is_ok(),
            binary_name: "tycode-subprocess".to_string(),
        },
        codex: check("codex"),
        claude: check("claude"),
        kiro: check("kiro-cli"),
        gemini: check("gemini"),
    }
}

#[tauri::command]
async fn query_backend_usage(
    state: tauri::State<'_, AppState>,
    backend_kind: String,
    host_id: Option<String>,
) -> Result<Value, String> {
    let transport = usage_transport_for_host_id(state.inner(), host_id.as_deref())?;
    usage::query_backend_usage_for_host(&backend_kind, transport).await
}

fn usage_transport_for_host_id(
    state: &AppState,
    host_id: Option<&str>,
) -> Result<BackendTransport, String> {
    let Some(id) = host_id else {
        return Ok(BackendTransport::Local);
    };
    let store = state.host_store.lock();
    let host = store
        .get(id)
        .ok_or_else(|| format!("Host '{id}' not found"))?;
    if host.is_local {
        Ok(BackendTransport::Local)
    } else {
        Ok(BackendTransport::from_ssh_host(Some(host.hostname.clone())))
    }
}

#[tauri::command]
fn set_disabled_backends(
    state: tauri::State<'_, AppState>,
    backends: Vec<String>,
) -> Result<(), String> {
    let mut disabled = state.disabled_backends.lock();
    disabled.clear();
    for b in backends {
        disabled.insert(b);
    }
    Ok(())
}

fn detect_local_target() -> Result<String, String> {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-musl"
    } else if cfg!(target_os = "windows") {
        "pc-windows-msvc"
    } else {
        return Err("Unsupported operating system".to_string());
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return Err("Unsupported architecture".to_string());
    };

    Ok(format!("{arch}-{os}"))
}

async fn install_tycode_subprocess() -> Result<(), String> {
    let target = detect_local_target()?;
    let archive = format!("{SUBPROCESS_CRATE_NAME}-{target}.tar.xz");
    let url = format!("{SUBPROCESS_GIT_REPO}/releases/download/v{SUBPROCESS_VERSION}/{archive}");

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "Could not determine home directory".to_string())?;
    let install_dir = format!("{home}/.tycode/v{SUBPROCESS_VERSION}/bin");

    let cmd = format!(
        "TMP=$(mktemp -d) && \
         curl -sSfL \"{url}\" | tar -xJ -C \"$TMP\" && \
         mkdir -p \"{install_dir}\" && \
         find \"$TMP\" -name \"{SUBPROCESS_CRATE_NAME}\" -type f -exec mv {{}} \"{install_dir}/{SUBPROCESS_CRATE_NAME}\" \\; && \
         chmod +x \"{install_dir}/{SUBPROCESS_CRATE_NAME}\" && \
         rm -rf \"$TMP\""
    );
    let output = tokio::process::Command::new("sh")
        .args(["-c", &cmd])
        .output()
        .await
        .map_err(|e| format!("Failed to run install command: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to install tycode-subprocess v{SUBPROCESS_VERSION} ({target}): {stderr}"
        ));
    }
    Ok(())
}

async fn install_codex() -> Result<(), String> {
    let output = tokio::process::Command::new("npm")
        .args(["install", "-g", "@openai/codex"])
        .output()
        .await
        .map_err(|e| format!("Failed to run npm: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to install codex: {stderr}"));
    }
    Ok(())
}

async fn install_claude_code() -> Result<(), String> {
    let output = tokio::process::Command::new("sh")
        .args(["-c", "curl -fsSL https://claude.ai/install.sh | bash"])
        .output()
        .await
        .map_err(|e| format!("Failed to run install command: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to install claude-code: {stderr}"));
    }
    Ok(())
}

async fn install_kiro() -> Result<(), String> {
    let output = tokio::process::Command::new("sh")
        .args([
            "-c",
            "curl -fsSL https://cli.kiro.dev/install | bash -s -- --force",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run install script: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to install kiro: {stderr}"));
    }
    Ok(())
}

async fn install_gemini() -> Result<(), String> {
    let output = tokio::process::Command::new("npm")
        .args(["install", "-g", "@google/gemini-cli"])
        .output()
        .await
        .map_err(|e| format!("Failed to run npm: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to install gemini-cli: {stderr}"));
    }
    Ok(())
}

#[tauri::command]
async fn install_backend_dependency(backend_kind: String) -> Result<(), String> {
    match backend_kind.as_str() {
        "tycode" => install_tycode_subprocess().await,
        "codex" => install_codex().await,
        "claude" => install_claude_code().await,
        "kiro" => install_kiro().await,
        "gemini" => install_gemini().await,
        other => Err(format!("Unknown backend kind: {other}")),
    }
}

#[tauri::command]
async fn create_conversation(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_roots: Vec<String>,
    backend_kind: Option<String>,
    ephemeral: Option<bool>,
    agent_definition_id: Option<String>,
    ui_owner_project_id: Option<String>,
) -> Result<CreateConversationResponse, String> {
    create_conversation_tauri_free(
        &app,
        state.inner(),
        workspace_roots,
        backend_kind,
        ephemeral,
        agent_definition_id,
        ui_owner_project_id,
    )
    .await
}

pub(crate) async fn create_conversation_tauri_free(
    app: &tauri::AppHandle,
    state: &AppState,
    workspace_roots: Vec<String>,
    backend_kind: Option<String>,
    ephemeral: Option<bool>,
    agent_definition_id: Option<String>,
    ui_owner_project_id: Option<String>,
) -> Result<CreateConversationResponse, String> {
    // Route through TydeServer if the host is configured for it.
    match host_router::route_workspace(app, state, &workspace_roots).await? {
        host_router::WorkspaceRoute::TydeServer { connection } => {
            return create_conversation_via_server(
                app,
                state,
                connection,
                workspace_roots,
                backend_kind,
                ephemeral,
                agent_definition_id,
                ui_owner_project_id,
            )
            .await;
        }
        host_router::WorkspaceRoute::Local => {}
    }

    // Resolve definition if provided
    let definition = if let Some(ref def_id) = agent_definition_id {
        let def_workspace = workspace_roots
            .iter()
            .find(|r| !r.starts_with("ssh://"))
            .map(|r| r.to_string());
        let defs = agent_defs_io::list_agent_definitions(def_workspace).await?;
        let found = defs.into_iter().find(|e| e.definition.id == *def_id);
        Some(
            found
                .ok_or_else(|| format!("Agent definition '{def_id}' not found"))?
                .definition,
        )
    } else {
        None
    };

    let include_agent_control = definition
        .as_ref()
        .map(|d| d.include_agent_control)
        .unwrap_or(false);
    let def_tool_policy = definition
        .as_ref()
        .map(|d| d.tool_policy.clone())
        .unwrap_or_default();

    // Use definition's default_backend if caller didn't specify one
    let effective_backend = if backend_kind.is_none() {
        definition.as_ref().and_then(|d| d.default_backend.clone())
    } else {
        backend_kind
    };
    let backend_kind = resolve_requested_backend_kind(state, effective_backend, &workspace_roots)?;
    let ephemeral = ephemeral.unwrap_or(false);

    // For agent-control conversations, reserve an agent_id upfront so we can embed it
    // in the MCP startup config. The MCP server uses this to auto-inject
    // parent_agent_id when spawning sub-agents.
    let reserved_agent_id = if include_agent_control {
        let mut runtime = state.agent_runtime.lock().await;
        Some(runtime.reserve_agent_id())
    } else {
        None
    };

    let launch_target = resolve_backend_launch_target(app, &workspace_roots, backend_kind).await?;

    // Build extra MCP servers from definition
    let extra_mcp_servers = definition
        .as_ref()
        .map(|d| definition_mcp_servers_to_startup(&d.mcp_servers))
        .unwrap_or_default();

    let startup_mcp_servers = startup_mcp_servers_for_agent(
        state,
        include_agent_control,
        reserved_agent_id.as_deref(),
        &workspace_roots,
        &extra_mcp_servers,
    )?;

    // Resolve and inject skills from ~/.tyde/skills/.
    let def_skill_names = definition
        .as_ref()
        .map(|d| d.skill_names.as_slice())
        .unwrap_or(&[]);
    let skill_injection = if !def_skill_names.is_empty() {
        let workspace_root_for_skills = workspace_roots
            .first()
            .map(|s| s.as_str())
            .ok_or("Skill injection requires a workspace root")?;
        let agent_id = definition
            .as_ref()
            .map(|d| d.id.as_str())
            .unwrap_or("unknown");
        Some(
            skill_injection::inject_skills_for_backend(
                backend_kind,
                &launch_target.transport,
                workspace_root_for_skills,
                agent_id,
                def_skill_names,
            )
            .await?,
        )
    } else {
        None
    };
    let skill_dir = skill_injection
        .as_ref()
        .and_then(|r| r.skill_dir.as_deref());

    // Build agent identity for backends that support native agent flags.
    // Instructions go through --agents/--agent (Claude) rather than
    // --append-system-prompt, so the model treats them as first-class identity.
    let agent_identity = definition.as_ref().and_then(|d| {
        let instr = d.instructions.as_deref().unwrap_or("").trim();
        if instr.is_empty() {
            None
        } else {
            Some(backend::AgentIdentity {
                id: d.id.clone(),
                description: d.description.clone(),
                instructions: instr.to_string(),
            })
        }
    });

    // Compose remaining steering: tool policy + workspace steering.
    // Agent instructions are NOT included here — they go through agent_identity.
    let workspace_steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let steering = compose_steering(None, &def_tool_policy, workspace_steering.as_deref());

    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &launch_target,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
        steering.as_deref(),
        agent_identity.as_ref(),
        skill_dir,
    )
    .await?;

    let id = {
        let mut mgr = state.manager.lock().await;
        mgr.create_conversation(session, &workspace_roots)
    };

    // Store skill cleanup handle for this conversation.
    if let Some(injection) = skill_injection {
        state.skill_cleanups.lock().insert(id, injection.cleanup);
    }

    let def_name = definition
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Conversation".to_string());

    // For agent-control conversations, complete the agent registration using the
    // reserved ID so it appears in the hierarchy and sub-agents can reference it.
    let root_agent = if let Some(ref agent_id) = reserved_agent_id {
        let mut runtime = state.agent_runtime.lock().await;
        let info = runtime.register_agent_with_id(
            agent_id.clone(),
            id,
            workspace_roots.clone(),
            backend_kind.as_str().to_string(),
            None,
            def_name.clone(),
            ui_owner_project_id.clone(),
        );
        runtime.update_agent_type(agent_id, agent_definition_id.clone());
        runtime.set_agent_definition(
            agent_id,
            agent_definition_id.clone(),
            def_tool_policy.clone(),
        );
        drop(runtime);
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(app, state, &info.agent_id).await;
        info
    } else {
        let info = ensure_conversation_agent_registered(
            app,
            state,
            id,
            &workspace_roots,
            backend_kind.as_str(),
            &def_name,
            ui_owner_project_id.clone(),
        )
        .await;
        if agent_definition_id.is_some() {
            let updated = {
                let mut runtime = state.agent_runtime.lock().await;
                runtime.update_agent_type(&info.agent_id, agent_definition_id.clone());
                runtime.set_agent_definition(
                    &info.agent_id,
                    agent_definition_id.clone(),
                    def_tool_policy.clone(),
                );
                runtime.get_agent(&info.agent_id)
            };
            if let Some(updated) = updated {
                emit_agent_changed(app, state, &updated.agent_id).await;
                updated
            } else {
                info
            }
        } else {
            info
        }
    };

    {
        let mgr = state.manager.lock().await;
        let session = mgr.get(id).ok_or("Conversation not found")?;
        session
            .set_subagent_emitter(Arc::new(BackendSubAgentEmitter {
                app: app.clone(),
                agent_runtime: state.agent_runtime.clone(),
                agent_runtime_notify: state.agent_runtime_notify.clone(),
                parent_agent_id: reserved_agent_id,
                lazy_parent_agent_id: Mutex::new(None),
                parent_conversation_id: id,
                workspace_roots: workspace_roots.clone(),
                backend_kind: backend_kind.as_str().to_string(),
                assistant_sender_name: backend_assistant_sender_name(backend_kind).to_string(),
                session_store: state.session_store.clone(),
                conversation_to_session: state.conversation_to_session.clone(),
            }))
            .await;
    }

    let registration = serde_json::json!({
        "kind": "ConversationRegistered",
        "data": {
            "agent_id": root_agent.agent_id,
            "workspace_roots": workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": &root_agent.name,
            "parent_agent_id": null,
            "ui_owner_project_id": root_agent.ui_owner_project_id,
        }
    });

    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(id, settings_tx.clone());
    }

    // Create session store record (skip for ephemeral conversations)
    let tyde_session_id = if !ephemeral {
        let workspace_root = workspace_roots.first().map(|s| s.as_str());
        let session_record = {
            let mut store = state.session_store.lock();
            store.create(backend_kind.as_str(), workspace_root)?
        };
        let sid = session_record.id.clone();
        {
            let mut map = state.conversation_to_session.lock();
            map.insert(id, sid.clone());
        }
        sid
    } else {
        String::new()
    };

    tokio::spawn(forward_events(
        app.clone(),
        id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
        settings_tx,
        state.session_store.clone(),
        state.conversation_to_session.clone(),
    ));
    Ok(CreateConversationResponse {
        conversation_id: id,
        session_id: tyde_session_id,
    })
}

async fn forward_events(
    app: tauri::AppHandle,
    conversation_id: u64,
    mut rx: mpsc::UnboundedReceiver<Value>,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    registration: Value,
    settings_tx: watch::Sender<Value>,
    session_store: Arc<SyncMutex<SessionStore>>,
    conversation_to_session: Arc<SyncMutex<HashMap<u64, String>>>,
) {
    // Emit the initial registration event for locally-spawned sessions.
    // Remote TydeServer sessions pass Value::Null — the server sends
    // ConversationRegistered via event replay through the same channel.
    if !registration.is_null() {
        let reg_payload = ChatEventPayload {
            conversation_id,
            event: registration,
        };
        if let Ok(debug_payload) = serde_json::to_value(&reg_payload) {
            record_debug_event_from_app(&app, "chat", debug_payload);
        }
        if let Err(e) = app.emit("chat-event", &reg_payload) {
            tracing::warn!("Failed to emit ConversationRegistered event: {e:?}");
        }
    }

    while let Some(event) = rx.recv().await {
        if event.get("kind").and_then(|k| k.as_str()) == Some("Settings") {
            if let Some(data) = event.get("data") {
                let _ = settings_tx.send(data.clone());
            }
        }

        // Session store updates based on event kind
        let event_kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let tyde_session_id = conversation_to_session
            .lock()
            .get(&conversation_id)
            .cloned();
        if let Some(ref sid) = tyde_session_id {
            match event_kind {
                "MessageAdded" => {
                    // Only count user messages to avoid double-counting (fix #8).
                    // MessageAdded fires for both user and assistant messages.
                    // StreamEnd fires once per assistant turn, so we count that separately.
                    let is_user = event
                        .get("data")
                        .and_then(|d| d.get("sender"))
                        .and_then(|s| s.as_str())
                        == Some("User");
                    if is_user {
                        if let Err(err) = session_store.lock().increment_message_count(sid) {
                            tracing::error!("Failed to increment message count: {err}");
                        }
                    }
                }
                "StreamEnd" => {
                    // Count assistant turn
                    if let Err(err) = session_store.lock().increment_message_count(sid) {
                        tracing::error!("Failed to increment message count: {err}");
                    }
                }
                "TaskUpdate" => {
                    if let Some(title) = event
                        .get("data")
                        .and_then(|d| d.get("title"))
                        .and_then(|t| t.as_str())
                    {
                        let trimmed = title.trim();
                        if !trimmed.is_empty() {
                            if let Err(err) = session_store.lock().set_alias(sid, trimmed) {
                                tracing::error!("Failed to set session alias: {err}");
                            }
                        }
                    }
                }
                "SessionStarted" => {
                    if let Some(session_id) = event
                        .get("data")
                        .and_then(|d| d.get("session_id"))
                        .and_then(|s| s.as_str())
                    {
                        if let Err(err) =
                            session_store.lock().set_backend_session_id(sid, session_id)
                        {
                            tracing::error!(
                                "Failed to set backend_session_id from SessionStarted: {err}"
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        let changed = {
            let mut runtime = agent_runtime.lock().await;
            runtime.record_chat_event(conversation_id, &event)
        };
        if changed {
            agent_runtime_notify.notify_waiters();
            let (info, agent_seq) = {
                let runtime = agent_runtime.lock().await;
                let info = runtime.get_agent_by_conversation(conversation_id);
                let seq = info
                    .as_ref()
                    .and_then(|agent| runtime.latest_event_seq_for_agent(&agent.agent_id));
                (info, seq)
            };
            if let Some(info) = info {
                let _ = app.emit("agent-changed", &info);
                if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
                    let _ = rc.event_broadcast.send(protocol::ServerFrame::Event {
                        event: "agent-changed".into(),
                        seq: agent_seq,
                        payload: serde_json::to_value(&info).unwrap_or_default(),
                    });
                }
            }
        }

        let payload = ChatEventPayload {
            conversation_id,
            event,
        };
        if let Ok(debug_payload) = serde_json::to_value(&payload) {
            record_debug_event_from_app(&app, "chat", debug_payload);
        }
        if let Err(e) = app.emit("chat-event", &payload) {
            tracing::warn!("Failed to emit chat event: {e:?}");
        }

        // Tap events into the remote control chat buffer + broadcast.
        if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
            let entry = rc
                .chat_buffer
                .lock()
                .push(conversation_id, payload.event.clone());
            let _ = rc.event_broadcast.send(protocol::ServerFrame::Event {
                event: "chat-event".into(),
                seq: Some(entry.seq),
                payload: serde_json::json!({
                    "conversation_id": conversation_id,
                    "event": payload.event,
                }),
            });
        }
    }

    if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
        rc.chat_buffer.lock().remove_conversation(conversation_id);
    }
}

fn emit_subprocess_exit(app: &tauri::AppHandle, conversation_id: u64) {
    let payload = ChatEventPayload {
        conversation_id,
        event: serde_json::json!({
            "kind": "SubprocessExit",
            "data": { "exit_code": serde_json::Value::Null },
        }),
    };
    if let Ok(debug_payload) = serde_json::to_value(&payload) {
        record_debug_event_from_app(app, "chat", debug_payload);
    }
    if let Err(err) = app.emit("chat-event", &payload) {
        tracing::warn!("Failed to emit synthetic SubprocessExit: {err:?}");
    }
}

async fn emit_agent_changed(app: &tauri::AppHandle, state: &AppState, agent_id: &str) {
    let (info, agent_seq) = {
        let runtime = state.agent_runtime.lock().await;
        (
            runtime.get_agent(agent_id),
            runtime.latest_event_seq_for_agent(agent_id),
        )
    };
    if let Some(info) = info {
        let _ = app.emit("agent-changed", &info);
        if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
            let _ = rc.event_broadcast.send(protocol::ServerFrame::Event {
                event: "agent-changed".into(),
                seq: agent_seq,
                payload: serde_json::to_value(&info).unwrap_or_default(),
            });
        }
    }
}

async fn emit_agent_changed_for_conversation(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
) {
    let (info, agent_seq) = {
        let runtime = state.agent_runtime.lock().await;
        let info = runtime.get_agent_by_conversation(conversation_id);
        let seq = info
            .as_ref()
            .and_then(|agent| runtime.latest_event_seq_for_agent(&agent.agent_id));
        (info, seq)
    };
    if let Some(info) = info {
        let _ = app.emit("agent-changed", &info);
        if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
            let _ = rc.event_broadcast.send(protocol::ServerFrame::Event {
                event: "agent-changed".into(),
                seq: agent_seq,
                payload: serde_json::to_value(&info).unwrap_or_default(),
            });
        }
    }
}

pub(crate) async fn execute_conversation_command(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
    command: SessionCommand,
) -> Result<(), String> {
    let handle = {
        let mgr = state.manager.lock().await;
        let session = mgr.get(conversation_id).ok_or("Conversation not found")?;
        session.command_handle()
    };

    match handle.execute(command).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let removed_session = {
                let mut mgr = state.manager.lock().await;
                mgr.remove(conversation_id)
            };
            if let Some(session) = removed_session {
                session.shutdown().await;
            }
            let changed = {
                let mut runtime = state.agent_runtime.lock().await;
                runtime.mark_conversation_failed(conversation_id, err.clone())
            };
            if changed {
                state.agent_runtime_notify.notify_waiters();
                emit_agent_changed_for_conversation(app, state, conversation_id).await;
            }
            // Clean up conversation_to_session map (store record persists)
            state
                .conversation_to_session
                .lock()
                .remove(&conversation_id);
            if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
                rc.chat_buffer.lock().remove_conversation(conversation_id);
            }
            emit_subprocess_exit(app, conversation_id);
            Err(err)
        }
    }
}

#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
    message: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::SendMessage { message, images },
    )
    .await
}

#[tauri::command]
async fn cancel_conversation(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::CancelConversation,
    )
    .await
}

#[tauri::command]
async fn close_conversation(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    close_conversation_tauri_free(&app, state.inner(), conversation_id).await
}

pub(crate) async fn close_conversation_tauri_free(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
) -> Result<(), String> {
    let session = {
        let mut mgr = state.manager.lock().await;
        mgr.remove(conversation_id)
            .ok_or("Conversation not found")?
    };
    session.shutdown().await;
    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_conversation_closed(conversation_id, Some("Conversation closed".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed_for_conversation(app, state, conversation_id).await;
    }
    cleanup_conversation_runtime_state(app, state, conversation_id).await;
    Ok(())
}

async fn cleanup_conversation_runtime_state(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
) {
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.remove(&conversation_id);
    }

    // Clean up runtime maps (store record persists)
    state
        .conversation_to_session
        .lock()
        .remove(&conversation_id);
    state.remote_chat_senders.lock().remove(&conversation_id);

    // Clean up remote control chat buffer for this conversation.
    if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
        rc.chat_buffer.lock().remove_conversation(conversation_id);
    }

    // Clean up injected skills for this conversation.
    let skill_cleanup = state.skill_cleanups.lock().remove(&conversation_id);
    if let Some(cleanup) = skill_cleanup {
        skill_injection::cleanup_injected_skills(cleanup).await;
    }
}

pub(crate) async fn spawn_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: SpawnAgentRequest,
) -> Result<SpawnAgentResponse, String> {
    // Route through TydeServer if the host is configured for it.
    match host_router::route_workspace(app, state, &request.workspace_roots).await? {
        host_router::WorkspaceRoute::TydeServer { connection } => {
            return spawn_agent_via_server(app, state, connection, request).await;
        }
        host_router::WorkspaceRoute::Local => {}
    }

    let SpawnAgentRequest {
        workspace_roots,
        prompt,
        backend_kind,
        parent_agent_id,
        ui_owner_project_id,
        name,
        ephemeral,
        images,
        agent_definition_id,
    } = request;

    if workspace_roots.iter().all(|root| root.trim().is_empty()) {
        return Err("spawn_agent requires at least one workspace root".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("spawn_agent requires a non-empty prompt".to_string());
    }

    if let Some(ref parent_id) = parent_agent_id {
        let exists = {
            let runtime = state.agent_runtime.lock().await;
            runtime.has_agent(parent_id)
        };
        if !exists {
            return Err(format!("Parent agent {parent_id} was not found"));
        }
    }

    // Resolve definition if provided
    let definition = if let Some(ref def_id) = agent_definition_id {
        let def_workspace = workspace_roots
            .iter()
            .find(|r| !r.starts_with("ssh://"))
            .map(|r| r.to_string());
        let defs = agent_defs_io::list_agent_definitions(def_workspace).await?;
        let found = defs.into_iter().find(|e| e.definition.id == *def_id);
        Some(
            found
                .ok_or_else(|| format!("Agent definition '{def_id}' not found"))?
                .definition,
        )
    } else {
        None
    };

    let include_agent_control = definition
        .as_ref()
        .map(|d| d.include_agent_control)
        .unwrap_or(false);
    let def_tool_policy = definition
        .as_ref()
        .map(|d| d.tool_policy.clone())
        .unwrap_or_default();

    let effective_backend = if backend_kind.is_none() {
        definition.as_ref().and_then(|d| d.default_backend.clone())
    } else {
        backend_kind
    };
    let backend_kind = resolve_requested_backend_kind(state, effective_backend, &workspace_roots)?;
    let ephemeral = ephemeral.unwrap_or(false);
    let launch_target = resolve_backend_launch_target(app, &workspace_roots, backend_kind).await?;

    let extra_mcp_servers = definition
        .as_ref()
        .map(|d| definition_mcp_servers_to_startup(&d.mcp_servers))
        .unwrap_or_default();

    let startup_mcp_servers = startup_mcp_servers_for_agent(
        state,
        include_agent_control,
        None,
        &workspace_roots,
        &extra_mcp_servers,
    )?;

    // Resolve and inject skills from ~/.tyde/skills/.
    let def_skill_names = definition
        .as_ref()
        .map(|d| d.skill_names.as_slice())
        .unwrap_or(&[]);
    let skill_injection = if !def_skill_names.is_empty() {
        let workspace_root_for_skills = workspace_roots
            .first()
            .map(|s| s.as_str())
            .ok_or("Skill injection requires a workspace root")?;
        let agent_id = definition
            .as_ref()
            .map(|d| d.id.as_str())
            .unwrap_or("unknown");
        Some(
            skill_injection::inject_skills_for_backend(
                backend_kind,
                &launch_target.transport,
                workspace_root_for_skills,
                agent_id,
                def_skill_names,
            )
            .await?,
        )
    } else {
        None
    };
    let skill_dir = skill_injection
        .as_ref()
        .and_then(|r| r.skill_dir.as_deref());

    let agent_identity = definition.as_ref().and_then(|d| {
        let instr = d.instructions.as_deref().unwrap_or("").trim();
        if instr.is_empty() {
            None
        } else {
            Some(backend::AgentIdentity {
                id: d.id.clone(),
                description: d.description.clone(),
                instructions: instr.to_string(),
            })
        }
    });

    let workspace_steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let steering = compose_steering(None, &def_tool_policy, workspace_steering.as_deref());

    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &launch_target,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
        steering.as_deref(),
        agent_identity.as_ref(),
        skill_dir,
    )
    .await?;

    let display_name = name.trim().to_string();
    if display_name.is_empty() {
        return Err("spawn_agent requires a non-empty name".to_string());
    }

    let conversation_id = {
        let mut mgr = state.manager.lock().await;
        mgr.create_conversation(session, &workspace_roots)
    };

    // Store skill cleanup handle for this conversation.
    if let Some(injection) = skill_injection {
        state
            .skill_cleanups
            .lock()
            .insert(conversation_id, injection.cleanup);
    }

    let info = {
        let mut runtime = state.agent_runtime.lock().await;
        let info = runtime.register_agent(
            conversation_id,
            workspace_roots.clone(),
            backend_kind.as_str().to_string(),
            parent_agent_id.clone(),
            display_name.clone(),
            ui_owner_project_id,
        );
        if agent_definition_id.is_some() {
            runtime.update_agent_type(&info.agent_id, agent_definition_id.clone());
            runtime.set_agent_definition(&info.agent_id, agent_definition_id, def_tool_policy);
        }
        info
    };
    state.agent_runtime_notify.notify_waiters();
    emit_agent_changed(app, state, &info.agent_id).await;

    // Create session store record for this agent (skip for ephemeral agents)
    if !ephemeral {
        let tyde_session_id = {
            let workspace_root = workspace_roots.first().map(|s| s.as_str());
            let record = state
                .session_store
                .lock()
                .create(backend_kind.as_str(), workspace_root)?;
            record.id
        };
        // Set parent_id if this agent has a parent
        if let Some(ref parent_runtime_agent_id) = parent_agent_id {
            let parent_cid = {
                let runtime = state.agent_runtime.lock().await;
                runtime.conversation_id_for_agent(parent_runtime_agent_id)
            };
            if let Some(parent_cid) = parent_cid {
                let parent_tyde_id = state
                    .conversation_to_session
                    .lock()
                    .get(&parent_cid)
                    .cloned();
                if let Some(parent_tyde_id) = parent_tyde_id {
                    state
                        .session_store
                        .lock()
                        .set_parent(&tyde_session_id, &parent_tyde_id)?;
                }
            }
        }
        // Set alias from display name if non-generic
        if !display_name.is_empty() && !is_generic_agent_name(&display_name) {
            state
                .session_store
                .lock()
                .set_alias(&tyde_session_id, &display_name)?;
        }
        state
            .conversation_to_session
            .lock()
            .insert(conversation_id, tyde_session_id);
    }

    // Set up the sub-agent emitter AFTER registration so we know this agent's id.
    // Sub-agents spawned by this agent will have parent_agent_id = info.agent_id.
    {
        let mgr = state.manager.lock().await;
        let session = mgr.get(conversation_id).ok_or("Conversation not found")?;
        session
            .set_subagent_emitter(Arc::new(BackendSubAgentEmitter {
                app: app.clone(),
                agent_runtime: state.agent_runtime.clone(),
                agent_runtime_notify: state.agent_runtime_notify.clone(),
                parent_agent_id: Some(info.agent_id.clone()),
                lazy_parent_agent_id: Mutex::new(None),
                parent_conversation_id: conversation_id,
                workspace_roots: workspace_roots.clone(),
                backend_kind: backend_kind.as_str().to_string(),
                assistant_sender_name: backend_assistant_sender_name(backend_kind).to_string(),
                session_store: state.session_store.clone(),
                conversation_to_session: state.conversation_to_session.clone(),
            }))
            .await;
    }

    let registration = serde_json::json!({
        "kind": "ConversationRegistered",
        "data": {
            "agent_id": &info.agent_id,
            "workspace_roots": workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": &info.name,
            "parent_agent_id": parent_agent_id,
            "ui_owner_project_id": info.ui_owner_project_id,
        }
    });

    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(conversation_id, settings_tx.clone());
    }

    tokio::spawn(forward_events(
        app.clone(),
        conversation_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
        settings_tx,
        state.session_store.clone(),
        state.conversation_to_session.clone(),
    ));

    execute_conversation_command(
        app,
        state,
        conversation_id,
        SessionCommand::SendMessage {
            message: prompt,
            images,
        },
    )
    .await?;

    Ok(SpawnAgentResponse {
        agent_id: info.agent_id,
        conversation_id,
    })
}

pub(crate) async fn send_agent_message_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: SendAgentMessageRequest,
) -> Result<(), String> {
    let SendAgentMessageRequest { agent_id, message } = request;
    if message.trim().is_empty() {
        return Err("send_agent_message requires a non-empty message".to_string());
    }

    // Forward to remote server if agent lives there.
    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        connection
            .invoke(
                "send_agent_message",
                serde_json::json!({
                    "agent_id": &agent_id,
                    "message": message,
                }),
            )
            .await?;
        return Ok(());
    }

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(&agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    execute_conversation_command(
        app,
        state,
        conversation_id,
        SessionCommand::SendMessage {
            message,
            images: None,
        },
    )
    .await?;

    Ok(())
}

pub(crate) async fn interrupt_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: AgentIdRequest,
) -> Result<(), String> {
    let agent_id = request.agent_id;

    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        connection
            .invoke(
                "interrupt_agent",
                serde_json::json!({ "agent_id": &agent_id }),
            )
            .await?;
        return Ok(());
    }

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(&agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    // Cascade interrupt to child agents first
    let child_ids: Vec<AgentId> = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .children_of(&agent_id)
            .iter()
            .filter(|c| c.is_running)
            .map(|c| c.agent_id.clone())
            .collect()
    };
    for child_id in child_ids {
        let _ = Box::pin(interrupt_agent_internal(
            app,
            state,
            AgentIdRequest { agent_id: child_id },
        ))
        .await;
    }

    let has_session = {
        let mgr = state.manager.lock().await;
        mgr.get(conversation_id).is_some()
    };
    if !has_session {
        let changed = {
            let mut runtime = state.agent_runtime.lock().await;
            runtime.mark_conversation_closed(conversation_id, Some("Cancelled".to_string()))
        };
        if changed {
            state.agent_runtime_notify.notify_waiters();
            emit_agent_changed(app, state, &agent_id).await;
        }
        return Ok(());
    }

    execute_conversation_command(
        app,
        state,
        conversation_id,
        SessionCommand::CancelConversation,
    )
    .await?;

    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_agent_running(&agent_id, Some("Cancelling...".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(app, state, &agent_id).await;
    }

    Ok(())
}

pub(crate) async fn terminate_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: AgentIdRequest,
) -> Result<(), String> {
    let agent_id = request.agent_id;

    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        connection
            .invoke(
                "terminate_agent",
                serde_json::json!({ "agent_id": &agent_id }),
            )
            .await?;

        // Best-effort local cleanup for mirrored proxy conversations.
        if let Some(local_cid) = connection.detach_remote_agent(&agent_id).await {
            let removed = {
                let mut mgr = state.manager.lock().await;
                mgr.remove(local_cid)
            };
            if let Some(session) = removed {
                // Remote already handled teardown; only shut down non-proxy sessions.
                match session {
                    BackendSession::TydeServer(_) => {}
                    other => other.shutdown().await,
                }
            }
            cleanup_conversation_runtime_state(app, state, local_cid).await;
        }

        dev_instance::stop_instances_for_agent(state, &agent_id).await;
        return Ok(());
    }

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(&agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    // Cascade termination to child agents first
    let child_ids: Vec<AgentId> = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .children_of(&agent_id)
            .iter()
            .filter(|c| c.is_running)
            .map(|c| c.agent_id.clone())
            .collect()
    };
    for child_id in child_ids {
        let _ = Box::pin(terminate_agent_internal(
            app,
            state,
            AgentIdRequest { agent_id: child_id },
        ))
        .await;
    }

    let session = {
        let mut mgr = state.manager.lock().await;
        mgr.remove(conversation_id)
    };
    if let Some(session) = session {
        session.shutdown().await;
    }

    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_conversation_closed(conversation_id, Some("Terminated".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(app, state, &agent_id).await;
    }

    cleanup_conversation_runtime_state(app, state, conversation_id).await;

    // Clean up any dev instances bound to this agent.
    dev_instance::stop_instances_for_agent(state, &agent_id).await;

    Ok(())
}

pub(crate) async fn get_agent_internal(
    state: &AppState,
    request: AgentIdRequest,
) -> Result<Option<AgentInfo>, String> {
    if let Some(local) = {
        let runtime = state.agent_runtime.lock().await;
        runtime.get_agent(&request.agent_id)
    } {
        return Ok(Some(local));
    }

    let mut found: Option<AgentInfo> = None;
    for conn in tyde_server_connections_snapshot(state) {
        match conn.fetch_remote_agents().await {
            Ok(remote_agents) => {
                if let Some(agent) = remote_agents
                    .into_iter()
                    .find(|a| a.agent_id == request.agent_id)
                {
                    if found.is_some() {
                        return Err(format!(
                            "Agent {} exists on multiple remote hosts (ambiguous owner)",
                            request.agent_id
                        ));
                    }
                    found = Some(agent);
                }
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to fetch remote agents for host {} while resolving get_agent({}): {}",
                    conn.host_id,
                    request.agent_id,
                    err
                );
            }
        }
    }

    Ok(found)
}

pub(crate) async fn list_agents_local_only_internal(state: &AppState) -> Vec<AgentInfo> {
    let runtime = state.agent_runtime.lock().await;
    runtime.list_agents()
}

fn tyde_server_connections_snapshot(
    state: &AppState,
) -> Vec<Arc<tyde_server_conn::TydeServerConnection>> {
    let conns = state.tyde_server_connections.lock();
    conns.values().cloned().collect()
}

pub(crate) async fn list_agents_internal(state: &AppState) -> Result<Vec<AgentInfo>, String> {
    let mut merged = list_agents_local_only_internal(state).await;
    let mut seen = HashSet::new();
    for agent in &merged {
        if !seen.insert(agent.agent_id.clone()) {
            return Err(format!(
                "Duplicate local agent id {} detected while listing agents",
                agent.agent_id
            ));
        }
    }

    for conn in tyde_server_connections_snapshot(state) {
        match conn.fetch_remote_agents().await {
            Ok(remote_agents) => {
                for agent in remote_agents {
                    if !seen.insert(agent.agent_id.clone()) {
                        return Err(format!(
                            "Agent {} exists in both local and remote runtimes (ambiguous owner)",
                            agent.agent_id
                        ));
                    }
                    merged.push(agent);
                }
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to fetch remote agents for host {} while listing agents: {}",
                    conn.host_id,
                    err
                );
            }
        }
    }

    Ok(merged)
}

pub(crate) async fn list_child_agents_internal(
    state: &AppState,
    parent_agent_id: &str,
) -> Result<Vec<AgentInfo>, String> {
    let mut children = list_agents_internal(state)
        .await?
        .into_iter()
        .filter(|agent| agent.parent_agent_id.as_deref() == Some(parent_agent_id))
        .collect::<Vec<_>>();
    children.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
    Ok(children)
}

pub(crate) async fn wait_for_agent_internal(
    state: &AppState,
    request: WaitForAgentRequest,
) -> Result<AgentInfo, String> {
    let agent_id = request.agent_id;

    // Forward to remote server if agent lives there
    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        let resp = connection
            .invoke(
                "wait_for_agent",
                serde_json::json!({
                    "agent_id": &agent_id,
                }),
            )
            .await?;
        let mut info: AgentInfo =
            serde_json::from_value(resp).map_err(|e| format!("Failed to parse agent info: {e}"))?;
        info.conversation_id = connection
            .to_local_conversation_id(info.conversation_id)
            .await?;
        connection
            .register_remote_agent_id(info.agent_id.clone())
            .await;
        return Ok(info);
    }

    loop {
        // Create the Notified future BEFORE checking state to avoid missing
        // a notification that fires between the state check and the await.
        let notified = state.agent_runtime_notify.notified();

        let current = {
            let runtime = state.agent_runtime.lock().await;
            runtime
                .get_agent(&agent_id)
                .ok_or(format!("Agent {agent_id} not found"))?
        };
        if !current.is_running {
            return Ok(current);
        }

        notified.await;
    }
}

pub(crate) async fn agent_events_since_internal(
    state: &AppState,
    request: AgentEventsSinceRequest,
) -> Result<AgentEventBatch, String> {
    let runtime = state.agent_runtime.lock().await;
    Ok(runtime.events_since(request.since_seq.unwrap_or(0), request.limit.unwrap_or(200)))
}

pub(crate) async fn collect_agent_result_internal(
    state: &AppState,
    request: AgentIdRequest,
) -> Result<CollectedAgentResult, String> {
    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &request.agent_id).await?
    {
        let resp = connection
            .invoke(
                "collect_agent_result",
                serde_json::json!({
                    "agent_id": &request.agent_id,
                }),
            )
            .await?;
        let mut result: CollectedAgentResult = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse collected result: {e}"))?;
        result.agent.conversation_id = connection
            .to_local_conversation_id(result.agent.conversation_id)
            .await?;
        connection
            .register_remote_agent_id(result.agent.agent_id.clone())
            .await;
        return Ok(result);
    }
    let runtime = state.agent_runtime.lock().await;
    runtime.collect_result(&request.agent_id)
}

fn agent_result_from_info(info: &AgentInfo) -> AgentResult {
    AgentResult {
        agent_id: info.agent_id.clone(),
        is_running: info.is_running,
        message: info.last_message.clone(),
        error: info.last_error.clone(),
        summary: info.summary.clone(),
    }
}

/// Spawn an agent, block until it becomes idle, and return its result.
pub(crate) async fn run_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: SpawnAgentRequest,
) -> Result<AgentResult, String> {
    let spawn_resp = spawn_agent_internal(app, state, request).await?;
    let wait_request = WaitForAgentRequest {
        agent_id: spawn_resp.agent_id,
    };
    let info = wait_for_agent_internal(state, wait_request).await?;
    Ok(agent_result_from_info(&info))
}

const QUERY_SCREENSHOT_PREAMBLE: &str = "\
You are a visual inspector for a Tyde dev instance. A screenshot of the current UI \
is attached to this message.

Your job is to answer a visual question about the UI based on the screenshot. \
Provide a concise, factual answer.

Guidelines:
- Be concise — your entire response will be returned as a text summary to another agent
- Focus on answering the specific question asked
- Describe what you see: layout, colors, spacing, visual state of elements
- Only answer based on what is visually apparent in the screenshot

Question: ";

/// Take a screenshot of the dev instance, then spawn an ephemeral agent with the
/// image attached to answer a visual question about the UI.
pub(crate) async fn run_query_screenshot_agent(
    app: &tauri::AppHandle,
    state: &AppState,
    instance_id: Option<u64>,
    question: String,
) -> Result<String, String> {
    // 1. Take a screenshot via the debug MCP proxy.
    let screenshot_result = dev_instance::proxy_debug_tool_call(
        state,
        instance_id,
        "tyde_debug_capture_screenshot",
        serde_json::json!({}),
    )
    .await?;

    // 2. Extract the base64 PNG data from the proxy response.
    let content_arr = screenshot_result
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or("Screenshot response missing content array")?;

    let mut png_base64: Option<String> = None;
    for item in content_arr {
        if let Some(json_text) = item.get("text").and_then(|t| t.as_str()) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_text) {
                if let Ok(data) = crate::debug_mcp_http::extract_valid_png_data(&meta) {
                    png_base64 = Some(data.to_string());
                    break;
                }
            }
        }
    }
    let png_base64 = png_base64.ok_or("Could not extract PNG data from screenshot response")?;

    // 3. Build the image attachment.
    let image_size = png_base64.len() as u64;
    let image = ImageAttachment {
        data: png_base64,
        media_type: "image/png".to_string(),
        name: "screenshot.png".to_string(),
        size: image_size,
    };

    // 4. Spawn an ephemeral agent with the screenshot attached.
    let prompt = format!("{QUERY_SCREENSHOT_PREAMBLE}{question}");
    let project_dir = dev_instance::dev_instance_project_dir(state, instance_id)
        .ok_or("No dev instance running")?;

    let request = SpawnAgentRequest {
        workspace_roots: vec![project_dir],
        prompt,
        backend_kind: None,
        parent_agent_id: None,
        ui_owner_project_id: None,
        name: "__internal_query_screenshot__".to_string(),
        ephemeral: Some(true),
        images: Some(vec![image]),
        agent_definition_id: None,
    };

    let spawn_resp = spawn_agent_internal(app, state, request).await?;
    let agent_id = spawn_resp.agent_id;

    let wait_result = wait_for_agent_internal(
        state,
        WaitForAgentRequest {
            agent_id: agent_id.clone(),
        },
    )
    .await;

    // Collect result and terminate regardless of wait outcome.
    let result = collect_agent_result_internal(
        state,
        AgentIdRequest {
            agent_id: agent_id.clone(),
        },
    )
    .await;

    let _ = terminate_agent_internal(app, state, AgentIdRequest { agent_id }).await;

    // If the wait itself failed (timeout), return that error.
    wait_result?;

    let collected = result?;
    collected
        .final_message
        .ok_or_else(|| "Query screenshot agent finished without producing a response".to_string())
}

/// epoll-style wait: block until any of the watched agents becomes idle.
/// Returns the idle agents and the list of still-running ones.
pub(crate) async fn await_agents_internal(
    state: &AppState,
    request: AwaitAgentsRequest,
    caller_agent_id: Option<&str>,
) -> Result<AwaitAgentsResponse, String> {
    let AwaitAgentsRequest {
        agent_ids,
        timeout_ms,
    } = request;

    if agent_ids.is_empty() {
        return Err("No agents to watch".to_string());
    }

    let mut watched_agent_ids = Vec::with_capacity(agent_ids.len());
    let mut seen_ids = HashSet::new();
    for raw_id in agent_ids {
        let id = raw_id.trim();
        if id.is_empty() {
            return Err("agent_ids cannot contain an empty value".to_string());
        }
        if !seen_ids.insert(id.to_string()) {
            return Err(format!("Duplicate agent id in watch list: {id}"));
        }
        watched_agent_ids.push(id.to_string());
    }

    let all_agents = list_agents_internal(state).await?;
    let mut by_id = HashMap::with_capacity(all_agents.len());
    for agent in all_agents {
        by_id.insert(agent.agent_id.clone(), agent);
    }

    if let Some(caller_id) = caller_agent_id {
        if !by_id.contains_key(caller_id) {
            return Err(format!("Caller agent {caller_id} not found"));
        }
        if watched_agent_ids.iter().any(|id| id == caller_id) {
            return Err(format!("Agent {caller_id} cannot await itself"));
        }
    }
    for watched_id in &watched_agent_ids {
        let watched = by_id
            .get(watched_id)
            .ok_or_else(|| format!("Agent {watched_id} not found"))?;
        if let Some(caller_id) = caller_agent_id {
            if watched.parent_agent_id.as_deref() != Some(caller_id) {
                return Err(format!(
                    "Agent {watched_id} is not a direct child of caller agent {caller_id}"
                ));
            }
        }
    }

    let idle_timeout = timeout_ms.unwrap_or(60_000).clamp(1, 30 * 60 * 1000);
    let idle_duration = tokio::time::Duration::from_millis(idle_timeout);
    let max_wall = idle_duration.saturating_mul(10);
    let wall_deadline = tokio::time::Instant::now() + max_wall;
    let mut idle_deadline = tokio::time::Instant::now() + idle_duration;
    let mut last_updated_at_ms: Option<u64> = None;

    loop {
        // Create the Notified future BEFORE checking state to avoid missing
        // a local-runtime notification that fires between the state check
        // and the await.
        let notified = state.agent_runtime_notify.notified();

        // Pull a unified local+remote snapshot so this works for TydeServer
        // agents too.
        let all_agents = list_agents_internal(state).await?;
        let mut by_id = HashMap::with_capacity(all_agents.len());
        for agent in all_agents {
            by_id.insert(agent.agent_id.clone(), agent);
        }

        let mut ready = Vec::new();
        let mut still_running = Vec::new();
        let mut newest_updated_at: u64 = 0;
        for id in &watched_agent_ids {
            let info = by_id
                .get(id.as_str())
                .ok_or(format!("Agent {id} not found"))?;
            if info.updated_at_ms > newest_updated_at {
                newest_updated_at = info.updated_at_ms;
            }
            if info.is_running {
                still_running.push(id.clone());
            } else {
                ready.push(agent_result_from_info(info));
            }
        }

        if !ready.is_empty() {
            return Ok(AwaitAgentsResponse {
                ready,
                still_running,
            });
        }

        // Extend idle deadline when any watched agent shows new activity.
        if last_updated_at_ms.is_none_or(|prev| newest_updated_at > prev) {
            idle_deadline = tokio::time::Instant::now() + idle_duration;
            last_updated_at_ms = Some(newest_updated_at);
        }

        let now = tokio::time::Instant::now();
        let effective_deadline = idle_deadline.min(wall_deadline);
        if now >= effective_deadline {
            return Err("Timed out waiting for agents".to_string());
        }
        let remaining = effective_deadline.saturating_duration_since(now);
        let poll_sleep = remaining.min(tokio::time::Duration::from_millis(500));
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(poll_sleep) => {}
        }
    }
}

/// Cancel an agent: interrupt it and shut down its subprocess.
pub(crate) async fn cancel_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: AgentIdRequest,
) -> Result<AgentResult, String> {
    let agent_id = request.agent_id;

    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        let response = connection
            .invoke(
                "cancel_agent",
                serde_json::json!({
                    "agent_id": &agent_id,
                }),
            )
            .await?;

        if let Some(local_cid) = connection.detach_remote_agent(&agent_id).await {
            let removed = {
                let mut mgr = state.manager.lock().await;
                mgr.remove(local_cid)
            };
            if let Some(session) = removed {
                // Remote already handled teardown; only shut down non-proxy sessions.
                match session {
                    BackendSession::TydeServer(_) => {}
                    other => other.shutdown().await,
                }
            }
            cleanup_conversation_runtime_state(app, state, local_cid).await;
        }

        dev_instance::stop_instances_for_agent(state, &agent_id).await;

        return serde_json::from_value(response)
            .map_err(|e| format!("Failed to parse remote cancel result: {e}"));
    }

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(&agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    // Send cancel signal first.
    let _ = execute_conversation_command(
        app,
        state,
        conversation_id,
        SessionCommand::CancelConversation,
    )
    .await;

    // Then tear down the session.
    let session = {
        let mut mgr = state.manager.lock().await;
        mgr.remove(conversation_id)
    };
    if let Some(session) = session {
        session.shutdown().await;
    }

    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_conversation_closed(conversation_id, Some("Cancelled".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(app, state, &agent_id).await;
    }

    cleanup_conversation_runtime_state(app, state, conversation_id).await;

    // Clean up any dev instances bound to this agent.
    dev_instance::stop_instances_for_agent(state, &agent_id).await;

    let runtime = state.agent_runtime.lock().await;
    let info = runtime
        .get_agent(&agent_id)
        .ok_or(format!("Agent {agent_id} not found"))?;
    Ok(agent_result_from_info(&info))
}

#[tauri::command]
async fn spawn_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<String>,
    parent_agent_id: Option<AgentId>,
    ui_owner_project_id: Option<String>,
    name: String,
    ephemeral: Option<bool>,
) -> Result<SpawnAgentResponse, String> {
    spawn_agent_internal(
        &app,
        state.inner(),
        SpawnAgentRequest {
            workspace_roots,
            prompt,
            backend_kind,
            parent_agent_id,
            ui_owner_project_id,
            name,
            ephemeral,
            images: None,
            agent_definition_id: None,
        },
    )
    .await
}

#[tauri::command]
async fn send_agent_message(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
    message: String,
) -> Result<(), String> {
    send_agent_message_internal(
        &app,
        state.inner(),
        SendAgentMessageRequest { agent_id, message },
    )
    .await
}

#[tauri::command]
async fn interrupt_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
) -> Result<(), String> {
    interrupt_agent_internal(&app, state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn terminate_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
) -> Result<(), String> {
    terminate_agent_internal(&app, state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn get_agent(
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
) -> Result<Option<AgentInfo>, String> {
    get_agent_internal(state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn rename_agent(
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
    name: String,
) -> Result<(), String> {
    rename_agent_tauri_free(state.inner(), agent_id, name).await
}

pub(crate) async fn rename_agent_tauri_free(
    state: &AppState,
    agent_id: AgentId,
    name: String,
) -> Result<(), String> {
    // Forward to remote server if agent lives there.
    if let host_router::AgentRoute::TydeServer { connection } =
        host_router::route_agent(state, &agent_id).await?
    {
        connection
            .invoke(
                "rename_agent",
                serde_json::json!({
                    "agent_id": &agent_id,
                    "name": name,
                }),
            )
            .await?;
        return Ok(());
    }

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime.conversation_id_for_agent(&agent_id)
    };
    {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.rename_agent(&agent_id, name.clone());
    }
    // Update session store alias (skip generic names so auto-titling can fire)
    if !is_generic_agent_name(&name) {
        if let Some(cid) = conversation_id {
            let tyde_session_id = state.conversation_to_session.lock().get(&cid).cloned();
            if let Some(sid) = tyde_session_id {
                state.session_store.lock().set_alias(&sid, &name)?;
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn list_agents(state: tauri::State<'_, AppState>) -> Result<Vec<AgentInfo>, String> {
    list_agents_internal(state.inner()).await
}

#[tauri::command]
async fn wait_for_agent(
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
) -> Result<AgentInfo, String> {
    wait_for_agent_internal(state.inner(), WaitForAgentRequest { agent_id }).await
}

#[tauri::command]
async fn agent_events_since(
    state: tauri::State<'_, AppState>,
    since_seq: Option<u64>,
    limit: Option<usize>,
) -> Result<AgentEventBatch, String> {
    agent_events_since_internal(state.inner(), AgentEventsSinceRequest { since_seq, limit }).await
}

#[tauri::command]
async fn collect_agent_result(
    state: tauri::State<'_, AppState>,
    agent_id: AgentId,
) -> Result<CollectedAgentResult, String> {
    collect_agent_result_internal(state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
fn get_mcp_http_server_settings(
    state: tauri::State<'_, AppState>,
) -> Result<McpHttpServerSettings, String> {
    let enabled = *state.mcp_http_enabled.lock();
    Ok(current_mcp_http_server_settings(enabled))
}

#[tauri::command]
fn set_mcp_http_server_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<McpHttpServerSettings, String> {
    *state.mcp_http_enabled.lock() = enabled;
    save_app_settings(&app_settings_from_state(&state))?;

    if enabled {
        if let Err(err) = agent_mcp_http::start_agent_mcp_http_server(&app) {
            *state.mcp_http_enabled.lock() = false;
            let _ = save_app_settings(&app_settings_from_state(&state));
            return Err(format!("Failed to start MCP HTTP server: {err}"));
        }
    } else {
        agent_mcp_http::stop_agent_mcp_http_server();
    }

    let status = current_mcp_http_server_settings(enabled);
    if enabled && !status.running {
        *state.mcp_http_enabled.lock() = false;
        let _ = save_app_settings(&app_settings_from_state(&state));
        return Err("Failed to start MCP HTTP server".to_string());
    }
    Ok(status)
}

#[tauri::command]
fn get_driver_mcp_http_server_settings(
    state: tauri::State<'_, AppState>,
) -> Result<DriverMcpHttpServerSettings, String> {
    let enabled = *state.driver_mcp_http_enabled.lock();
    let autoload = *state.driver_mcp_http_autoload.lock();
    Ok(current_driver_mcp_http_server_settings(enabled, autoload))
}

#[tauri::command]
fn set_driver_mcp_http_server_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<DriverMcpHttpServerSettings, String> {
    if !enabled {
        *state.driver_mcp_http_autoload.lock() = false;
    }
    *state.driver_mcp_http_enabled.lock() = enabled;
    save_app_settings(&app_settings_from_state(&state))?;

    if enabled {
        if let Err(err) = driver_mcp_http::start_driver_mcp_http_server(&app) {
            *state.driver_mcp_http_enabled.lock() = false;
            let _ = save_app_settings(&app_settings_from_state(&state));
            return Err(format!("Failed to start driver MCP HTTP server: {err}"));
        }
    } else {
        driver_mcp_http::stop_driver_mcp_http_server();
    }

    let driver_autoload = *state.driver_mcp_http_autoload.lock();
    let status = current_driver_mcp_http_server_settings(enabled, driver_autoload);
    if enabled && !status.running {
        *state.driver_mcp_http_enabled.lock() = false;
        *state.driver_mcp_http_autoload.lock() = false;
        let _ = save_app_settings(&app_settings_from_state(&state));
        return Err("Failed to start driver MCP HTTP server".to_string());
    }
    Ok(status)
}

#[tauri::command]
fn set_driver_mcp_http_server_autoload_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<DriverMcpHttpServerSettings, String> {
    let driver_enabled = *state.driver_mcp_http_enabled.lock();
    if enabled && !driver_enabled {
        return Err("Enable driver MCP server before enabling auto-load".to_string());
    }

    let autoload = enabled && driver_enabled;
    *state.driver_mcp_http_autoload.lock() = autoload;
    save_app_settings(&app_settings_from_state(&state))?;

    Ok(current_driver_mcp_http_server_settings(
        driver_enabled,
        autoload,
    ))
}

#[tauri::command]
fn set_mcp_control_enabled(state: tauri::State<'_, AppState>, enabled: bool) -> Result<(), String> {
    *state.mcp_control_enabled.lock() = enabled;
    save_app_settings(&app_settings_from_state(&state))
}

#[derive(Debug, Clone, Serialize)]
struct RemoteControlSettings {
    enabled: bool,
    running: bool,
    socket_path: Option<String>,
    connected_clients: usize,
}

#[tauri::command]
async fn get_remote_control_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<RemoteControlSettings, String> {
    let enabled = *state.remote_control_enabled.lock();
    let (running, socket_path, connected_clients) =
        if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
            if rc.is_running() {
                (
                    true,
                    Some(rc.socket_path().display().to_string()),
                    rc.connected_client_count().await,
                )
            } else {
                (false, None, 0)
            }
        } else {
            (false, None, 0)
        };
    Ok(RemoteControlSettings {
        enabled,
        running,
        socket_path,
        connected_clients,
    })
}

#[tauri::command]
async fn set_remote_control_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<RemoteControlSettings, String> {
    *state.remote_control_enabled.lock() = enabled;
    save_app_settings(&app_settings_from_state(&state))?;

    if enabled {
        if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
            if let Err(err) = rc.start_listening(app.clone()) {
                *state.remote_control_enabled.lock() = false;
                let _ = save_app_settings(&app_settings_from_state(&state));
                return Err(format!("Failed to start remote control server: {err}"));
            }
        } else {
            match remote_control::RemoteControlServer::start(app.clone()) {
                Ok(server) => {
                    app.manage(server);
                }
                Err(err) => {
                    *state.remote_control_enabled.lock() = false;
                    let _ = save_app_settings(&app_settings_from_state(&state));
                    return Err(format!("Failed to start remote control server: {err}"));
                }
            }
        }
    } else if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
        rc.shutdown();
    }

    get_remote_control_settings(app, state).await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoteTydeServerState {
    NotInstalled,
    Stopped,
    RunningCurrent,
    RunningStale,
    RunningUnknown,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct RemoteTydeServerStatus {
    host_id: String,
    host: String,
    state: RemoteTydeServerState,
    local_version: String,
    remote_version: Option<String>,
    target: Option<String>,
    socket_path: Option<String>,
    install_path: Option<String>,
    installed_versions: Vec<String>,
    installed_client_version: bool,
    running: bool,
    needs_upgrade: bool,
    error: Option<String>,
}

fn resolve_remote_tyde_server_host(state: &AppState, host_id: &str) -> Result<host::Host, String> {
    let store = state.host_store.lock();
    let host = store.get(host_id).cloned().or_else(|| {
        store
            .list()
            .into_iter()
            .find(|entry| !entry.is_local && entry.hostname == host_id)
    });
    let host = host.ok_or_else(|| format!("Host '{host_id}' not found"))?;
    if host.is_local {
        return Err("Remote Tyde server actions are not valid for the local host".to_string());
    }
    if host.remote_kind != host::RemoteKind::TydeServer {
        return Err(format!(
            "Host '{}' is configured for SSH Pipe, not Tyde Server",
            host.hostname
        ));
    }
    Ok(host)
}

fn remote_tyde_error_status(host: &host::Host, error: String) -> RemoteTydeServerStatus {
    RemoteTydeServerStatus {
        host_id: host.id.clone(),
        host: host.hostname.clone(),
        state: RemoteTydeServerState::Error,
        local_version: crate::protocol::TYDE_VERSION.to_string(),
        remote_version: None,
        target: None,
        socket_path: None,
        install_path: None,
        installed_versions: Vec::new(),
        installed_client_version: false,
        running: false,
        needs_upgrade: false,
        error: Some(error),
    }
}

fn remote_tyde_state_for_status(status: &RemoteTydeServerStatus) -> RemoteTydeServerState {
    if status.running {
        if let Some(remote) = &status.remote_version {
            if remote == &status.local_version {
                return RemoteTydeServerState::RunningCurrent;
            }
            return RemoteTydeServerState::RunningStale;
        }
        return RemoteTydeServerState::RunningUnknown;
    }
    if status.installed_client_version {
        RemoteTydeServerState::Stopped
    } else {
        RemoteTydeServerState::NotInstalled
    }
}

fn should_upgrade_remote_tyde(
    local_version: &str,
    remote_version: Option<&str>,
    installed_client_version: bool,
) -> bool {
    if !installed_client_version {
        return true;
    }
    let Some(remote) = remote_version else {
        return false;
    };
    match compare_numeric_versions(remote, local_version) {
        Some(std::cmp::Ordering::Less) => true,
        Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater) => false,
        None => remote != local_version,
    }
}

async fn collect_remote_tyde_server_status(host: &host::Host) -> RemoteTydeServerStatus {
    let local_version = crate::protocol::TYDE_VERSION.to_string();

    let home = match resolve_remote_home_dir(&host.hostname).await {
        Ok(home) => home,
        Err(err) => {
            return remote_tyde_error_status(
                host,
                format!("Failed to resolve remote home directory: {err}"),
            );
        }
    };

    let socket_path = tyde_socket_path_from_home(&home);
    let target = match detect_remote_tyde_target(&host.hostname).await {
        Ok(target) => target,
        Err(err) => {
            return remote_tyde_error_status(
                host,
                format!("Failed to detect remote target: {err}"),
            );
        }
    };

    let install_path = crate::remote::tyde_install_path_from_home(&home, &local_version, &target);
    let installed_versions = match list_remote_tyde_installed_versions(&host.hostname).await {
        Ok(versions) => versions,
        Err(err) => {
            return remote_tyde_error_status(
                host,
                format!("Failed to read remote Tyde installs: {err}"),
            );
        }
    };

    let installed_client_version =
        match check_remote_tyde_install(&host.hostname, &local_version, &target).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(err) => {
                return remote_tyde_error_status(
                    host,
                    format!("Failed to check client Tyde install on remote host: {err}"),
                );
            }
        };

    let running = match is_remote_tyde_server_running(&host.hostname, &socket_path).await {
        Ok(running) => running,
        Err(err) => {
            return remote_tyde_error_status(
                host,
                format!("Failed to check remote Tyde socket status: {err}"),
            );
        }
    };

    let mut status = RemoteTydeServerStatus {
        host_id: host.id.clone(),
        host: host.hostname.clone(),
        state: RemoteTydeServerState::Error,
        local_version: local_version.clone(),
        remote_version: None,
        target: Some(target.clone()),
        socket_path: Some(socket_path),
        install_path: Some(install_path.clone()),
        installed_versions,
        installed_client_version,
        running,
        needs_upgrade: false,
        error: None,
    };

    if running {
        let connect_binary = if installed_client_version {
            Some(install_path)
        } else {
            match resolve_remote_tyde_from_path(&host.hostname).await {
                Ok(path) => path,
                Err(err) => {
                    status.error = Some(format!(
                        "Could not resolve remote Tyde binary in PATH: {err}"
                    ));
                    None
                }
            }
        };
        if let Some(binary) = connect_binary {
            match query_remote_tyde_server_version(&host.hostname, &binary, &local_version).await {
                Ok(version) => status.remote_version = Some(version),
                Err(err) => {
                    status.error = Some(format!(
                        "Could not query running remote Tyde version: {err}"
                    ))
                }
            }
        } else if status.error.is_none() {
            status.error =
                Some("Could not find a Tyde binary to query running server version".to_string());
        }
    }

    status.needs_upgrade = should_upgrade_remote_tyde(
        &status.local_version,
        status.remote_version.as_deref(),
        status.installed_client_version,
    );
    status.state = remote_tyde_state_for_status(&status);
    status
}

#[tauri::command(rename_all = "snake_case")]
async fn get_remote_tyde_server_status(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> Result<RemoteTydeServerStatus, String> {
    let host = resolve_remote_tyde_server_host(state.inner(), &host_id)?;
    Ok(collect_remote_tyde_server_status(&host).await)
}

#[tauri::command(rename_all = "snake_case")]
async fn install_remote_tyde_server(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> Result<RemoteTydeServerStatus, String> {
    let host = resolve_remote_tyde_server_host(state.inner(), &host_id)?;
    install_remote_tyde_binary(&host.hostname, crate::protocol::TYDE_VERSION).await?;
    Ok(collect_remote_tyde_server_status(&host).await)
}

#[tauri::command(rename_all = "snake_case")]
async fn launch_remote_tyde_server(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> Result<RemoteTydeServerStatus, String> {
    let host = resolve_remote_tyde_server_host(state.inner(), &host_id)?;
    let target = detect_remote_tyde_target(&host.hostname).await?;
    let install_path =
        check_remote_tyde_install(&host.hostname, crate::protocol::TYDE_VERSION, &target)
            .await?
            .ok_or_else(|| {
                format!(
                    "Tyde v{} is not installed on '{}'. Install it first.",
                    crate::protocol::TYDE_VERSION,
                    host.hostname
                )
            })?;
    launch_remote_tyde_headless(&host.hostname, &install_path).await?;
    state.tyde_server_connections.lock().remove(&host.id);
    Ok(collect_remote_tyde_server_status(&host).await)
}

#[tauri::command(rename_all = "snake_case")]
async fn install_and_launch_remote_tyde_server(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> Result<RemoteTydeServerStatus, String> {
    let host = resolve_remote_tyde_server_host(state.inner(), &host_id)?;
    let install_path =
        install_remote_tyde_binary(&host.hostname, crate::protocol::TYDE_VERSION).await?;
    launch_remote_tyde_headless(&host.hostname, &install_path).await?;
    state.tyde_server_connections.lock().remove(&host.id);
    Ok(collect_remote_tyde_server_status(&host).await)
}

#[tauri::command(rename_all = "snake_case")]
async fn upgrade_remote_tyde_server(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> Result<RemoteTydeServerStatus, String> {
    let host = resolve_remote_tyde_server_host(state.inner(), &host_id)?;
    let install_path =
        install_remote_tyde_binary(&host.hostname, crate::protocol::TYDE_VERSION).await?;
    stop_remote_tyde_headless(&host.hostname).await?;
    launch_remote_tyde_headless(&host.hostname, &install_path).await?;
    state.tyde_server_connections.lock().remove(&host.id);
    Ok(collect_remote_tyde_server_status(&host).await)
}

#[tauri::command]
fn list_hosts(state: tauri::State<'_, AppState>) -> Result<Vec<host::Host>, String> {
    let store = state.host_store.lock();
    Ok(store.list())
}

#[tauri::command(rename_all = "snake_case")]
async fn add_host(
    state: tauri::State<'_, AppState>,
    label: String,
    hostname: String,
    remote_kind: Option<String>,
) -> Result<host::Host, String> {
    let remote_kind = match remote_kind.as_deref() {
        Some("tyde_server") => host::RemoteKind::TydeServer,
        _ => host::RemoteKind::SshPipe,
    };

    // Validate SSH connectivity before accepting the host.
    let output = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=10",
            "-o",
            "BatchMode=yes",
            "-T",
            &hostname,
            "echo",
            "tycode-ok",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "SSH connection to '{}' failed: {}",
            hostname,
            stderr.trim()
        ));
    }

    let mut store = state.host_store.lock();
    store.add(label, hostname, remote_kind)
}

#[tauri::command]
fn remove_host(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let mut store = state.host_store.lock();
    store.remove(&id)
}

#[tauri::command]
fn update_host_label(
    state: tauri::State<'_, AppState>,
    id: String,
    label: String,
) -> Result<(), String> {
    let mut store = state.host_store.lock();
    store.update_label(&id, label)
}

#[tauri::command]
fn update_host_enabled_backends(
    state: tauri::State<'_, AppState>,
    id: String,
    backends: Vec<String>,
) -> Result<(), String> {
    let mut store = state.host_store.lock();
    store.update_enabled_backends(&id, backends)
}

#[tauri::command]
fn update_host_default_backend(
    state: tauri::State<'_, AppState>,
    id: String,
    backend: String,
) -> Result<(), String> {
    let mut store = state.host_store.lock();
    store.update_default_backend(&id, backend)
}

#[tauri::command]
fn get_host_for_workspace(
    state: tauri::State<'_, AppState>,
    workspace_path: String,
) -> Result<host::Host, String> {
    let store = state.host_store.lock();
    if let Some(remote) = parse_remote_path(&workspace_path) {
        for h in store.list() {
            if !h.is_local && h.hostname == remote.host {
                return Ok(h);
            }
        }
        return Err(format!("Remote host '{}' is not registered", remote.host));
    }
    store
        .get("local")
        .cloned()
        .ok_or_else(|| "Local host not found".to_string())
}

#[tauri::command]
fn submit_debug_ui_response(
    state: tauri::State<'_, AppState>,
    request_id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
) -> Result<(), String> {
    let sender = {
        let mut pending = state.debug_ui_pending.lock();
        pending.remove(&request_id)
    };

    let response = if ok {
        Ok(result.unwrap_or(Value::Null))
    } else {
        Err(error.unwrap_or_else(|| "UI action failed".to_string()))
    };

    record_debug_event(
        state.inner(),
        "ui_response",
        serde_json::json!({
            "request_id": request_id,
            "ok": ok,
            "result": summarize_value_for_debug(&response.clone().unwrap_or(Value::Null)),
            "error": response.clone().err(),
        }),
    );

    if let Some(tx) = sender {
        let _ = tx.send(response);
    }
    Ok(())
}

#[tauri::command]
async fn get_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(&app, &state, conversation_id, SessionCommand::GetSettings).await
}

#[tauri::command]
async fn list_sessions(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(&app, &state, conversation_id, SessionCommand::ListSessions).await
}

#[tauri::command]
async fn resume_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
    session_id: String,
) -> Result<(), String> {
    // Map conversation_to_session BEFORE sending the command so that
    // forward_events can route session store updates to the right record.
    // The conversation was created with ephemeral=true so no duplicate
    // record exists.
    let (backend_kind, workspace_root) = {
        let mgr = state.manager.lock().await;
        (
            mgr.backend_kind(conversation_id)
                .map(|k| k.as_str().to_string()),
            mgr.workspace_roots(conversation_id)
                .and_then(|r| r.first())
                .map(|s| s.to_string()),
        )
    };
    if let Some(ref bk) = backend_kind {
        let mut store = state.session_store.lock();
        let tyde_id = if let Some(existing) = store.get_by_backend_session(bk, &session_id) {
            existing.id.clone()
        } else {
            // External session being resumed for the first time — create the record
            // with the correct backend_session_id upfront.
            let record = store.create(bk, workspace_root.as_deref())?;
            store.set_backend_session_id(&record.id, &session_id)?;
            record.id
        };
        state
            .conversation_to_session
            .lock()
            .insert(conversation_id, tyde_id);
    }

    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::ResumeSession {
            session_id: session_id.clone(),
        },
    )
    .await
}

#[tauri::command]
async fn delete_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
    session_id: String,
) -> Result<(), String> {
    let backend_kind = {
        let mgr = state.manager.lock().await;
        mgr.backend_kind(conversation_id)
            .map(|k| k.as_str().to_string())
    };
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::DeleteSession {
            session_id: session_id.clone(),
        },
    )
    .await?;
    // Clean up session store: find the record by backend_session_id and delete it
    if let Some(ref bk) = backend_kind {
        let mut store = state.session_store.lock();
        if let Some(record) = store.get_by_backend_session(bk, &session_id) {
            let tyde_id = record.id.clone();
            store.delete(&tyde_id)?;
        }
    }
    Ok(())
}

#[tauri::command]
async fn get_session_id(
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<Option<String>, String> {
    let tyde_session_id = state
        .conversation_to_session
        .lock()
        .get(&conversation_id)
        .cloned();
    let Some(sid) = tyde_session_id else {
        return Ok(None);
    };
    let mut store = state.session_store.lock();
    Ok(store.get(&sid).and_then(|r| r.backend_session_id.clone()))
}

#[tauri::command]
async fn list_profiles(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(&app, &state, conversation_id, SessionCommand::ListProfiles).await
}

#[tauri::command]
async fn switch_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
    profile_name: String,
) -> Result<(), String> {
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::SwitchProfile { profile_name },
    )
    .await
}

#[tauri::command]
async fn get_module_schemas(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::GetModuleSchemas,
    )
    .await
}

#[tauri::command]
async fn list_models(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    execute_conversation_command(&app, &state, conversation_id, SessionCommand::ListModels).await
}

#[tauri::command]
async fn update_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
    settings: Value,
    persist: Option<bool>,
) -> Result<(), String> {
    let persist = persist.unwrap_or(false);

    // Only tycode supports GetSettings → Settings event round-trip.
    // Other backends (codex, claude, kiro) get a direct pass-through.
    let is_tycode = {
        let mgr = state.manager.lock().await;
        mgr.backend_kind(conversation_id) == Some(BackendKind::Tycode)
    };

    if !is_tycode {
        return execute_conversation_command(
            &app,
            &state,
            conversation_id,
            SessionCommand::UpdateSettings { settings, persist },
        )
        .await;
    }

    // Tycode: read-modify-write so we don't clobber unrelated fields.
    // The subprocess replaces its entire session state with whatever we send.
    // 1. Subscribe to the settings watch *before* requesting, so we don't miss the response.
    let mut settings_rx = {
        let watchers = state.settings_watch.lock().await;
        let tx = watchers
            .get(&conversation_id)
            .ok_or("Settings watch not found for conversation")?;
        tx.subscribe()
    };
    // Mark current value as seen so changed() waits for the next send.
    settings_rx.borrow_and_update();

    // 2. Ask the subprocess for its current settings.
    execute_conversation_command(&app, &state, conversation_id, SessionCommand::GetSettings)
        .await?;

    // 3. Wait for the Settings event to come back through forward_events.
    settings_rx
        .changed()
        .await
        .map_err(|_| "Settings watch channel closed")?;
    let current = settings_rx.borrow_and_update().clone();

    // 4. Merge the caller's patch on top of the current settings.
    let merged = match (current, settings) {
        (Value::Object(mut base), Value::Object(patch)) => {
            for (k, v) in patch {
                base.insert(k, v);
            }
            Value::Object(base)
        }
        (_, patch) => patch,
    };

    // 5. Write back the merged settings.
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::UpdateSettings {
            settings: merged,
            persist,
        },
    )
    .await
}

#[derive(Serialize, Clone)]
struct AdminEventPayload {
    admin_id: u64,
    event: Value,
}

#[tauri::command]
async fn create_admin_subprocess(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_roots: Vec<String>,
    backend_kind: Option<String>,
) -> Result<u64, String> {
    let backend_kind = resolve_requested_backend_kind(&state, backend_kind, &workspace_roots)?;
    let launch_target = resolve_backend_launch_target(&app, &workspace_roots, backend_kind).await?;
    let (session, rx) =
        BackendSession::spawn_admin(backend_kind, &launch_target, &workspace_roots).await?;

    let id = {
        let mut mgr = state.admin.lock().await;
        mgr.create(session)
    };

    tokio::spawn(forward_admin_events(app, id, rx));
    Ok(id)
}

async fn forward_admin_events(
    app: tauri::AppHandle,
    admin_id: u64,
    mut rx: mpsc::UnboundedReceiver<Value>,
) {
    while let Some(event) = rx.recv().await {
        let payload = AdminEventPayload { admin_id, event };
        if let Ok(debug_payload) = serde_json::to_value(&payload) {
            record_debug_event_from_app(&app, "admin", debug_payload);
        }
        if let Err(e) = app.emit("admin-event", &payload) {
            tracing::warn!("Failed to emit admin event: {e:?}");
        }
    }
}

async fn execute_admin_command(
    state: &tauri::State<'_, AppState>,
    admin_id: u64,
    command: SessionCommand,
) -> Result<(), String> {
    let handle = {
        let mgr = state.admin.lock().await;
        let session = mgr.get(admin_id).ok_or("Admin subprocess not found")?;
        session.command_handle()
    };
    match handle.execute(command).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let removed_session = {
                let mut mgr = state.admin.lock().await;
                mgr.remove(admin_id)
            };
            if let Some(session) = removed_session {
                session.shutdown().await;
            }
            Err(err)
        }
    }
}

#[tauri::command]
async fn close_admin_subprocess(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
) -> Result<(), String> {
    let session = {
        let mut mgr = state.admin.lock().await;
        mgr.remove(admin_id).ok_or("Admin subprocess not found")?
    };
    session.shutdown().await;
    Ok(())
}

#[tauri::command]
async fn admin_list_sessions(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
) -> Result<(), String> {
    execute_admin_command(&state, admin_id, SessionCommand::ListSessions).await
}

#[tauri::command]
async fn admin_get_settings(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
) -> Result<(), String> {
    execute_admin_command(&state, admin_id, SessionCommand::GetSettings).await
}

#[tauri::command]
async fn admin_update_settings(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
    settings: Value,
) -> Result<(), String> {
    execute_admin_command(
        &state,
        admin_id,
        SessionCommand::UpdateSettings {
            settings,
            persist: true,
        },
    )
    .await
}

#[tauri::command]
async fn admin_list_profiles(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
) -> Result<(), String> {
    execute_admin_command(&state, admin_id, SessionCommand::ListProfiles).await
}

#[tauri::command]
async fn admin_switch_profile(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
    profile_name: String,
) -> Result<(), String> {
    execute_admin_command(
        &state,
        admin_id,
        SessionCommand::SwitchProfile { profile_name },
    )
    .await
}

#[tauri::command]
async fn admin_get_module_schemas(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
) -> Result<(), String> {
    execute_admin_command(&state, admin_id, SessionCommand::GetModuleSchemas).await
}

#[tauri::command]
async fn admin_delete_session(
    state: tauri::State<'_, AppState>,
    admin_id: u64,
    session_id: String,
) -> Result<(), String> {
    let backend_kind = {
        let admin = state.admin.lock().await;
        admin.get(admin_id).map(|s| s.kind().as_str().to_string())
    };
    execute_admin_command(
        &state,
        admin_id,
        SessionCommand::DeleteSession {
            session_id: session_id.clone(),
        },
    )
    .await?;
    // Clean up session store
    if let Some(ref bk) = backend_kind {
        let mut store = state.session_store.lock();
        if let Some(record) = store.get_by_backend_session(bk, &session_id) {
            let tyde_id = record.id.clone();
            store.delete(&tyde_id)?;
        }
    }
    Ok(())
}

#[tauri::command]
async fn list_session_records(
    state: tauri::State<'_, AppState>,
    workspace_root: Option<String>,
) -> Result<Vec<session_store::SessionRecord>, String> {
    // If the workspace is remote, return only the remote server's records.
    if let Some(ref root) = workspace_root {
        if let Some(remote) = parse_remote_path(root) {
            let conns: Vec<Arc<tyde_server_conn::TydeServerConnection>> = state
                .tyde_server_connections
                .lock()
                .values()
                .cloned()
                .collect();
            for conn in &conns {
                if conn.ssh_host() == remote.host {
                    return conn.fetch_session_records().await.map_err(|err| {
                        format!(
                            "Failed to fetch session records from {}: {err}",
                            conn.host_id
                        )
                    });
                }
            }
            return Err(format!(
                "No TydeServer connection for remote host '{}'",
                remote.host
            ));
        }
    }
    // Local workspace: return only local store records.
    state.session_store.lock().list()
}

#[tauri::command]
async fn rename_session(
    state: tauri::State<'_, AppState>,
    id: String,
    name: String,
) -> Result<(), String> {
    // Route to TydeServer if the session belongs to a remote connection
    let conns: Vec<Arc<tyde_server_conn::TydeServerConnection>> = state
        .tyde_server_connections
        .lock()
        .values()
        .cloned()
        .collect();
    for conn in &conns {
        if conn.owns_session_record(&id).await {
            conn.invoke(
                "rename_session",
                serde_json::json!({ "id": id, "name": name }),
            )
            .await?;
            conn.fetch_session_records().await.map_err(|err| {
                format!("Renamed session but failed to refresh cached records: {err}")
            })?;
            return Ok(());
        }
    }
    let mut store = state.session_store.lock();
    store.set_user_alias(&id, &name)
}

#[tauri::command]
async fn set_session_alias(
    state: tauri::State<'_, AppState>,
    id: String,
    alias: String,
) -> Result<(), String> {
    let conns: Vec<Arc<tyde_server_conn::TydeServerConnection>> = state
        .tyde_server_connections
        .lock()
        .values()
        .cloned()
        .collect();
    for conn in &conns {
        if conn.owns_session_record(&id).await {
            conn.invoke(
                "set_session_alias",
                serde_json::json!({ "id": id, "alias": alias }),
            )
            .await?;
            conn.fetch_session_records()
                .await
                .map_err(|err| format!("Set alias but failed to refresh cached records: {err}"))?;
            return Ok(());
        }
    }
    let mut store = state.session_store.lock();
    store.set_alias(&id, &alias)
}

#[tauri::command]
async fn delete_session_record(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    // Route to TydeServer if the session belongs to a remote connection
    let conns: Vec<Arc<tyde_server_conn::TydeServerConnection>> = state
        .tyde_server_connections
        .lock()
        .values()
        .cloned()
        .collect();
    for conn in &conns {
        if conn.owns_session_record(&id).await {
            conn.invoke("delete_session_record", serde_json::json!({ "id": id }))
                .await?;
            conn.fetch_session_records().await.map_err(|err| {
                format!("Deleted session but failed to refresh cached records: {err}")
            })?;
            return Ok(());
        }
    }

    // Local: look up the record, delete from backend via temp admin subprocess,
    // then remove from the store.
    let (backend_session_id, backend_kind_str, workspace_root) = {
        let mut store = state.session_store.lock();
        let record = store
            .get(&id)
            .ok_or_else(|| format!("Session record '{id}' not found"))?;
        (
            record.backend_session_id.clone(),
            record.backend_kind.clone(),
            record.workspace_root.clone(),
        )
    };

    if let Some(ref bsid) = backend_session_id {
        let roots: Vec<String> = workspace_root.iter().cloned().collect();
        let backend_kind = resolve_requested_backend_kind(&state, Some(backend_kind_str), &roots)?;
        let launch_target = resolve_backend_launch_target(&app, &roots, backend_kind).await?;
        let (session, _rx) =
            BackendSession::spawn_admin(backend_kind, &launch_target, &roots).await?;
        let handle = session.command_handle();
        let result = handle
            .execute(SessionCommand::DeleteSession {
                session_id: bsid.clone(),
            })
            .await;
        session.shutdown().await;
        result?;
    }

    state.session_store.lock().delete(&id)
}

// ---------------------------------------------------------------------------
// Project store commands
// ---------------------------------------------------------------------------

pub(crate) fn emit_projects_changed(app: &tauri::AppHandle, state: &AppState) {
    let projects = match state.project_store.lock().list() {
        Ok(p) => p,
        Err(err) => {
            tracing::error!("Failed to read project store for change event: {err}");
            return;
        }
    };
    let payload = serde_json::json!({ "projects": projects });
    let _ = app.emit("tyde-projects-changed", &payload);
    // Also push through the remote control broadcast channel so remote clients
    // receive the update.
    if let Some(rc) = app.try_state::<remote_control::RemoteControlServer>() {
        let _ = rc.event_broadcast.send(protocol::ServerFrame::Event {
            event: "tyde-projects-changed".into(),
            seq: None,
            payload,
        });
    }
}

#[tauri::command]
async fn list_projects(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
) -> Result<Vec<project_store::ProjectRecord>, String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        let records = conn.fetch_projects().await?;
        Ok(records
            .into_iter()
            .map(|r| conn.normalize_project_record(r))
            .collect())
    } else {
        state.project_store.lock().list()
    }
}

#[tauri::command]
async fn add_project(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
    workspace_path: String,
    name: String,
) -> Result<project_store::ProjectRecord, String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        let remote_workspace_path =
            host_router::strip_ssh_roots(std::slice::from_ref(&workspace_path))
                .into_iter()
                .next()
                .ok_or("Failed to resolve remote workspace path")?;
        let resp = conn
            .invoke(
                "add_project",
                serde_json::json!({
                    "workspace_path": remote_workspace_path,
                    "name": name
                }),
            )
            .await?;
        let record: project_store::ProjectRecord = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse remote project record: {e}"))?;
        Ok(conn.normalize_project_record(record))
    } else {
        let record = state.project_store.lock().add(&workspace_path, &name)?;
        emit_projects_changed(&app, &state);
        Ok(record)
    }
}

#[tauri::command]
async fn add_project_workbench(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
    parent_project_id: String,
    workspace_path: String,
    name: String,
    kind: String,
) -> Result<project_store::ProjectRecord, String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        let remote_workspace_path =
            host_router::strip_ssh_roots(std::slice::from_ref(&workspace_path))
                .into_iter()
                .next()
                .ok_or("Failed to resolve remote workspace path")?;
        let resp = conn
            .invoke(
                "add_project_workbench",
                serde_json::json!({
                    "parent_project_id": parent_project_id,
                    "workspace_path": remote_workspace_path,
                    "name": name,
                    "kind": kind
                }),
            )
            .await?;
        let record: project_store::ProjectRecord = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse remote project record: {e}"))?;
        Ok(conn.normalize_project_record(record))
    } else {
        let record = state.project_store.lock().add_workbench(
            &parent_project_id,
            &workspace_path,
            &name,
            &kind,
        )?;
        emit_projects_changed(&app, &state);
        Ok(record)
    }
}

#[tauri::command]
async fn remove_project(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
    id: String,
) -> Result<(), String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        conn.invoke("remove_project", serde_json::json!({ "id": id }))
            .await?;
        Ok(())
    } else {
        state.project_store.lock().remove(&id)?;
        emit_projects_changed(&app, &state);
        Ok(())
    }
}

#[tauri::command]
async fn rename_project(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
    id: String,
    name: String,
) -> Result<(), String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        conn.invoke(
            "rename_project",
            serde_json::json!({ "id": id, "name": name }),
        )
        .await?;
        Ok(())
    } else {
        state.project_store.lock().rename(&id, &name)?;
        emit_projects_changed(&app, &state);
        Ok(())
    }
}

#[tauri::command]
async fn update_project_roots(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    host: Option<String>,
    id: String,
    roots: Vec<String>,
) -> Result<(), String> {
    if let Some(host_id) = host {
        let conn = host_router::get_server_connection_by_id(&app, &state, &host_id).await?;
        conn.invoke(
            "update_project_roots",
            serde_json::json!({ "id": id, "roots": host_router::strip_ssh_roots(&roots) }),
        )
        .await?;
        Ok(())
    } else {
        state.project_store.lock().update_roots(&id, roots)?;
        emit_projects_changed(&app, &state);
        Ok(())
    }
}

#[tauri::command]
async fn discover_git_repos(workspace_dir: String) -> Result<Vec<String>, String> {
    if parse_remote_path(&workspace_dir).is_some() {
        return Ok(vec![workspace_dir]);
    }
    git_service::discover_git_repos(&workspace_dir).await
}

#[tauri::command]
async fn git_current_branch(working_dir: String) -> Result<String, String> {
    git_service::git_current_branch(&working_dir).await
}

#[tauri::command]
async fn git_status(working_dir: String) -> Result<Vec<GitFileStatus>, String> {
    git_service::git_status(&working_dir).await
}

#[tauri::command]
async fn git_stage(working_dir: String, paths: Vec<String>) -> Result<(), String> {
    git_service::git_stage(&working_dir, &paths).await
}

#[tauri::command]
async fn git_unstage(working_dir: String, paths: Vec<String>) -> Result<(), String> {
    git_service::git_unstage(&working_dir, &paths).await
}

#[tauri::command]
async fn git_commit(working_dir: String, message: String) -> Result<String, String> {
    git_service::git_commit(&working_dir, &message).await
}

#[tauri::command]
async fn git_diff(working_dir: String, path: String, staged: bool) -> Result<String, String> {
    git_service::git_diff(&working_dir, &path, staged).await
}

#[tauri::command]
async fn git_diff_base_content(
    working_dir: String,
    path: String,
    staged: bool,
) -> Result<String, String> {
    git_service::git_diff_base_content(&working_dir, &path, staged).await
}

#[tauri::command]
async fn git_worktree_add(working_dir: String, path: String, branch: String) -> Result<(), String> {
    git_service::git_worktree_add(&working_dir, &path, &branch).await
}

#[tauri::command]
async fn git_worktree_remove(working_dir: String, path: String) -> Result<(), String> {
    git_service::git_worktree_remove(&working_dir, &path).await
}

#[tauri::command]
async fn git_discard(working_dir: String, paths: Vec<String>) -> Result<(), String> {
    git_service::git_discard(&working_dir, &paths).await
}

#[tauri::command]
async fn list_directory(path: String, show_hidden: bool) -> Result<Vec<FileEntry>, String> {
    file_service::list_directory(&path, show_hidden).await
}

#[tauri::command]
async fn read_file_content(path: String) -> Result<FileContent, String> {
    file_service::read_file_content(&path).await
}

#[tauri::command]
fn sync_file_watch_paths(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<(), String> {
    let local_paths: Vec<String> = paths
        .into_iter()
        .filter(|path| parse_remote_path(path).is_none())
        .collect();

    let mut guard = state.file_watch.lock();

    if guard.is_none() {
        *guard = Some(FileWatchManager::new(app)?);
    }

    if let Some(manager) = guard.as_mut() {
        manager.sync_paths(&local_paths);
    }

    Ok(())
}

#[tauri::command]
fn watch_workspace_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    if parse_remote_path(&path).is_some() {
        return Ok(());
    }

    let mut guard = state.file_watch.lock();

    if guard.is_none() {
        *guard = Some(FileWatchManager::new(app)?);
    }

    if let Some(manager) = guard.as_mut() {
        manager.watch_dir(&path);
    }

    Ok(())
}

#[tauri::command]
fn unwatch_workspace_dir(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.file_watch.lock();

    if let Some(manager) = guard.as_mut() {
        manager.unwatch_dir();
    }

    Ok(())
}

#[tauri::command]
async fn create_terminal(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_path: String,
) -> Result<u64, String> {
    let mut mgr = state.terminals.lock().await;
    let terminal_id = mgr.create_session(app.clone(), &workspace_path)?;
    record_debug_event_from_app(
        &app,
        "terminal_created",
        serde_json::json!({
            "terminal_id": terminal_id,
            "workspace_path": workspace_path,
        }),
    );
    Ok(terminal_id)
}

#[tauri::command]
async fn write_terminal(
    state: tauri::State<'_, AppState>,
    terminal_id: u64,
    data: String,
) -> Result<(), String> {
    let mgr = state.terminals.lock().await;
    mgr.write(terminal_id, &data)
}

#[tauri::command]
async fn resize_terminal(
    state: tauri::State<'_, AppState>,
    terminal_id: u64,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let mgr = state.terminals.lock().await;
    mgr.resize(terminal_id, cols, rows)
}

#[tauri::command]
async fn close_terminal(state: tauri::State<'_, AppState>, terminal_id: u64) -> Result<(), String> {
    let mut mgr = state.terminals.lock().await;
    let result = mgr.close(terminal_id);
    if result.is_ok() {
        record_debug_event(
            state.inner(),
            "terminal_closed",
            serde_json::json!({
                "terminal_id": terminal_id,
            }),
        );
    }
    result
}

#[tauri::command]
async fn restart_subprocess(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    let (workspace_roots, backend_kind, old_session) = {
        let mut mgr = state.manager.lock().await;
        let roots = mgr
            .workspace_roots(conversation_id)
            .ok_or("Conversation not found")?
            .to_vec();
        let kind = mgr
            .backend_kind(conversation_id)
            .ok_or("Conversation not found")?;
        let session = mgr
            .remove(conversation_id)
            .ok_or("Conversation not found")?;
        (roots, kind, session)
    };

    old_session.shutdown().await;

    let launch_target = resolve_backend_launch_target(&app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers =
        startup_mcp_servers_for_new_sessions(state.inner(), false, &workspace_roots)?;
    let steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &launch_target,
        &workspace_roots,
        false,
        &startup_mcp_servers,
        steering.as_deref(),
        None,
        None,
    )
    .await?;

    {
        let mut mgr = state.manager.lock().await;
        mgr.insert(conversation_id, session, workspace_roots.clone());
    }

    let root_agent = ensure_conversation_agent_registered(
        &app,
        state.inner(),
        conversation_id,
        &workspace_roots,
        backend_kind.as_str(),
        "Conversation",
        None,
    )
    .await;

    let registration = serde_json::json!({
        "kind": "ConversationRegistered",
        "data": {
            "agent_id": root_agent.agent_id,
            "workspace_roots": &workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": &root_agent.name,
            "parent_agent_id": null,
            "ui_owner_project_id": root_agent.ui_owner_project_id,
        }
    });

    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(conversation_id, settings_tx.clone());
    }

    tokio::spawn(forward_events(
        app,
        conversation_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
        settings_tx,
        state.session_store.clone(),
        state.conversation_to_session.clone(),
    ));
    Ok(())
}

#[tauri::command]
async fn relaunch_conversation(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<(), String> {
    // 1. Look up the backend_session_id from the session store so we can resume
    //    the same session on the newly spawned backend. This is None when the
    //    backend crashed before emitting SessionStarted — in that case we simply
    //    start fresh (nothing to resume).
    let backend_session_id: Option<String> = {
        let tyde_session_id = state
            .conversation_to_session
            .lock()
            .get(&conversation_id)
            .cloned();
        tyde_session_id.and_then(|sid| {
            let mut store = state.session_store.lock();
            store.get(&sid).and_then(|r| r.backend_session_id.clone())
        })
    };

    // 2. Extract workspace_roots + backend_kind, then remove + shutdown old session.
    let (workspace_roots, backend_kind, old_session) = {
        let mut mgr = state.manager.lock().await;
        let roots = mgr
            .workspace_roots(conversation_id)
            .ok_or("Conversation not found")?
            .to_vec();
        let kind = mgr
            .backend_kind(conversation_id)
            .ok_or("Conversation not found")?;
        let session = mgr
            .remove(conversation_id)
            .ok_or("Conversation not found")?;
        (roots, kind, session)
    };

    old_session.shutdown().await;

    // 3. Spawn a fresh backend process.
    let launch_target = resolve_backend_launch_target(&app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers =
        startup_mcp_servers_for_new_sessions(state.inner(), false, &workspace_roots)?;
    let steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &launch_target,
        &workspace_roots,
        false,
        &startup_mcp_servers,
        steering.as_deref(),
        None,
        None,
    )
    .await?;

    // 4. Re-insert into ConversationManager under the same conversation_id.
    {
        let mut mgr = state.manager.lock().await;
        mgr.insert(conversation_id, session, workspace_roots.clone());
    }

    // 5. Re-register agent in AgentRuntime.
    let root_agent = ensure_conversation_agent_registered(
        &app,
        state.inner(),
        conversation_id,
        &workspace_roots,
        backend_kind.as_str(),
        "Conversation",
        None,
    )
    .await;

    let registration = serde_json::json!({
        "kind": "ConversationRegistered",
        "data": {
            "agent_id": root_agent.agent_id,
            "workspace_roots": &workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": &root_agent.name,
            "parent_agent_id": null,
            "ui_owner_project_id": root_agent.ui_owner_project_id,
        }
    });

    // 6. Set up settings watch and event forwarding.
    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(conversation_id, settings_tx.clone());
    }

    tokio::spawn(forward_events(
        app.clone(),
        conversation_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
        settings_tx,
        state.session_store.clone(),
        state.conversation_to_session.clone(),
    ));

    // 7. Resume the old session on the new backend (skip when there is nothing
    //    to resume, e.g. the backend crashed before SessionStarted).
    if let Some(session_id) = backend_session_id {
        execute_conversation_command(
            &app,
            &state,
            conversation_id,
            SessionCommand::ResumeSession { session_id },
        )
        .await?;
    }

    Ok(())
}

#[tauri::command]
async fn list_active_conversations(state: tauri::State<'_, AppState>) -> Result<Vec<u64>, String> {
    let mgr = state.manager.lock().await;
    Ok(mgr.active_ids())
}

#[tauri::command]
async fn shutdown_all_subprocesses(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conversations = {
        let mut mgr = state.manager.lock().await;
        mgr.drain_all()
    };
    let admins = {
        let mut mgr = state.admin.lock().await;
        mgr.drain_all()
    };
    let terminal_count = {
        let mgr = state.terminals.lock().await;
        mgr.len()
    };

    let count = conversations.len() + admins.len() + terminal_count;
    if count > 0 {
        tracing::info!("Shutting down {count} orphaned subprocesses/terminals");
    }

    for session in conversations.into_iter().chain(admins) {
        session.shutdown().await;
    }

    // Clean up all conversation_to_session mappings (store records persist)
    state.conversation_to_session.lock().clear();
    state.remote_chat_senders.lock().clear();

    {
        let mut mgr = state.terminals.lock().await;
        mgr.close_all();
    }
    Ok(())
}

#[tauri::command]
async fn submit_feedback(feedback: String) -> Result<(), String> {
    let client = reqwest::Client::new();
    let params = [("entry.515008519", feedback.as_str())];
    let res = client
        .post("https://docs.google.com/forms/d/e/1FAIpQLSfcaoYqtm0FRdibE5qJhVYONUbKAMn6KTIopx40Fk8l9yn2vA/formResponse")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to send feedback: {e}"))?;

    if !res.status().is_success() {
        return Err(format!(
            "Feedback submission failed with status {}",
            res.status()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn raise_fd_limit() {
    let hard = rlimit::getrlimit(rlimit::Resource::NOFILE)
        .map(|(_, hard)| hard)
        .unwrap_or(10240);
    let _ = rlimit::setrlimit(rlimit::Resource::NOFILE, hard, hard);
}

/// Resolves the user's login shell PATH and sets it process-wide.
/// macOS GUI apps launched from Dock/Finder inherit launchd's minimal PATH
/// (/usr/bin:/bin:/usr/sbin:/sbin), missing Homebrew, Cargo, nvm, etc.
/// Linux desktop launchers can have the same problem.
fn resolve_shell_path() {
    if cfg!(target_os = "windows") {
        return;
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

    let output = match Command::new(&shell)
        .args(["-li", "-c", "echo $PATH"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("Failed to resolve shell PATH via {shell}: {e:?}");
            return;
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "Shell PATH resolution exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if resolved.is_empty() {
        tracing::warn!("Shell PATH resolution returned empty string");
        return;
    }

    tracing::info!("Resolved shell PATH: {resolved}");
    std::env::set_var("PATH", &resolved);
}

// --- Workflow commands ---

#[tauri::command]
async fn list_workflows(
    workspace_path: Option<String>,
) -> Result<Vec<workflow_io::WorkflowEntry>, String> {
    workflow_io::list_workflows(workspace_path).await
}

#[tauri::command]
async fn save_workflow(
    workflow_json: String,
    scope: String,
    workspace_path: Option<String>,
) -> Result<(), String> {
    workflow_io::save_workflow(&workflow_json, &scope, workspace_path).await
}

#[tauri::command]
async fn delete_workflow(
    id: String,
    scope: String,
    workspace_path: Option<String>,
) -> Result<(), String> {
    workflow_io::delete_workflow(&id, &scope, workspace_path).await
}

#[tauri::command]
async fn run_shell_command(
    command: String,
    cwd: String,
) -> Result<workflow_io::ShellCommandResult, String> {
    workflow_io::run_shell_command(&command, &cwd).await
}

// --- Agent definition commands ---

enum AgentDefinitionRoute {
    Local,
    TydeServer {
        connection: Arc<tyde_server_conn::TydeServerConnection>,
        remote_workspace_path: Option<String>,
    },
}

async fn route_agent_definition_command(
    app: &tauri::AppHandle,
    state: &AppState,
    workspace_path: Option<String>,
) -> Result<AgentDefinitionRoute, String> {
    let Some(workspace_path) = workspace_path else {
        return Ok(AgentDefinitionRoute::Local);
    };

    if parse_remote_path(&workspace_path).is_none() {
        return Ok(AgentDefinitionRoute::Local);
    }

    let roots = vec![workspace_path];
    match host_router::route_workspace(app, state, &roots).await? {
        host_router::WorkspaceRoute::Local => Ok(AgentDefinitionRoute::Local),
        host_router::WorkspaceRoute::TydeServer { connection } => {
            Ok(AgentDefinitionRoute::TydeServer {
                connection,
                remote_workspace_path: host_router::strip_ssh_roots(&roots).into_iter().next(),
            })
        }
    }
}

#[tauri::command]
async fn list_agent_definitions(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_path: Option<String>,
) -> Result<Vec<agent_defs_io::AgentDefinitionEntry>, String> {
    let local_workspace_path = workspace_path
        .clone()
        .filter(|path| parse_remote_path(path).is_none());
    match route_agent_definition_command(&app, state.inner(), workspace_path.clone()).await? {
        AgentDefinitionRoute::Local => {
            agent_defs_io::list_agent_definitions(local_workspace_path).await
        }
        AgentDefinitionRoute::TydeServer {
            connection,
            remote_workspace_path,
        } => {
            let response = connection
                .invoke(
                    "list_agent_definitions",
                    serde_json::json!({
                        "workspace_path": remote_workspace_path,
                    }),
                )
                .await?;
            serde_json::from_value(response)
                .map_err(|e| format!("Failed to parse remote agent definitions: {e}"))
        }
    }
}

#[tauri::command]
async fn save_agent_definition(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    definition_json: String,
    scope: String,
    workspace_path: Option<String>,
) -> Result<(), String> {
    let local_workspace_path = workspace_path
        .clone()
        .filter(|path| parse_remote_path(path).is_none());
    match route_agent_definition_command(&app, state.inner(), workspace_path.clone()).await? {
        AgentDefinitionRoute::Local => {
            agent_defs_io::save_agent_definition(&definition_json, &scope, local_workspace_path)
                .await
        }
        AgentDefinitionRoute::TydeServer {
            connection,
            remote_workspace_path,
        } => {
            connection
                .invoke(
                    "save_agent_definition",
                    serde_json::json!({
                        "definition_json": definition_json,
                        "scope": scope,
                        "workspace_path": remote_workspace_path,
                    }),
                )
                .await?;
            Ok(())
        }
    }
}

#[tauri::command]
async fn delete_agent_definition(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    scope: String,
    workspace_path: Option<String>,
) -> Result<(), String> {
    let local_workspace_path = workspace_path
        .clone()
        .filter(|path| parse_remote_path(path).is_none());
    match route_agent_definition_command(&app, state.inner(), workspace_path.clone()).await? {
        AgentDefinitionRoute::Local => {
            agent_defs_io::delete_agent_definition(&id, &scope, local_workspace_path).await
        }
        AgentDefinitionRoute::TydeServer {
            connection,
            remote_workspace_path,
        } => {
            connection
                .invoke(
                    "delete_agent_definition",
                    serde_json::json!({
                        "id": id,
                        "scope": scope,
                        "workspace_path": remote_workspace_path,
                    }),
                )
                .await?;
            Ok(())
        }
    }
}

#[tauri::command]
async fn list_available_skills() -> Result<Vec<String>, String> {
    skill_injection::list_available_skills()
}

#[cfg(target_os = "linux")]
fn detect_system_dark_mode() -> bool {
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.portal.Desktop",
            "--print-reply=literal",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            "string:org.freedesktop.appearance",
            "string:color-scheme",
        ])
        .output();
    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            // color-scheme: 1 = dark, 2 = light, 0 = no preference
            text.contains("uint32 1")
        }
        Err(_) => false,
    }
}

pub fn run_with_options(headless: bool) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    #[cfg(unix)]
    raise_fd_limit();

    resolve_shell_path();
    if !headless {
        #[cfg(target_os = "linux")]
        if detect_system_dark_mode() {
            std::env::set_var("GTK_THEME", "Adwaita:dark");
        }
    }
    if headless {
        tracing::info!("Starting Tyde in headless mode");
    }
    let driver_mcp_http_env_override = std::env::var("TYDE_DRIVER_MCP_HTTP_ENABLED").is_ok();
    let mut app_settings = load_app_settings();
    if !app_settings.driver_mcp_http_enabled {
        app_settings.driver_mcp_http_autoload = false;
    }
    // In headless mode, always enable remote control
    if headless {
        app_settings.remote_control_enabled = true;
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            manager: Mutex::new(ConversationManager::new()),
            admin: Mutex::new(AdminManager::new()),
            terminals: Mutex::new(TerminalManager::new()),
            file_watch: SyncMutex::new(None),
            agent_runtime: Arc::new(Mutex::new(AgentRuntime::new())),
            agent_runtime_notify: Arc::new(Notify::new()),
            session_store: Arc::new(SyncMutex::new({
                let path = resolve_tyde_app_settings_path()
                    .map(|p| p.parent().unwrap().join("session-store.json"))
                    .unwrap_or_else(|_| PathBuf::from("session-store.json"));
                SessionStore::load(path).expect("failed to load session store")
            })),
            project_store: Arc::new(SyncMutex::new({
                let path = resolve_tyde_app_settings_path()
                    .map(|p| p.parent().unwrap().join("projects.json"))
                    .unwrap_or_else(|_| PathBuf::from("projects.json"));
                ProjectStore::load(path).expect("failed to load project store")
            })),
            host_store: SyncMutex::new({
                let path = resolve_tyde_app_settings_path()
                    .map(|p| p.parent().unwrap().join("hosts.json"))
                    .unwrap_or_else(|_| PathBuf::from("hosts.json"));
                host::HostStore::load(path).expect("failed to load host store")
            }),
            conversation_to_session: Arc::new(SyncMutex::new(HashMap::new())),
            remote_chat_senders: Arc::new(SyncMutex::new(HashMap::new())),
            mcp_http_enabled: SyncMutex::new(app_settings.mcp_http_enabled),
            mcp_control_enabled: SyncMutex::new(app_settings.mcp_control_enabled),
            driver_mcp_http_enabled: SyncMutex::new(app_settings.driver_mcp_http_enabled),
            driver_mcp_http_autoload: SyncMutex::new(app_settings.driver_mcp_http_autoload),
            driver_mcp_http_env_override,
            remote_control_enabled: SyncMutex::new(app_settings.remote_control_enabled),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
            disabled_backends: SyncMutex::new(HashSet::new()),
            settings_watch: Mutex::new(HashMap::new()),
            dev_instances: SyncMutex::new(dev_instance::DevInstanceRegistry::new()),
            tyde_server_connections: SyncMutex::new(HashMap::new()),
            skill_cleanups: SyncMutex::new(HashMap::new()),
        })
        .setup(move |app| {
            if !headless {
                initialize_tray(app)?;
            }
            let mcp_http_enabled = *app.state::<AppState>().mcp_http_enabled.lock();
            if mcp_http_enabled {
                if let Err(err) = agent_mcp_http::start_agent_mcp_http_server(app.handle()) {
                    tracing::warn!("Agent MCP HTTP server failed to start: {err}");
                }
            } else {
                tracing::info!("Agent MCP HTTP server disabled by app settings");
            }
            // Debug MCP server is only started on dev instances via TYDE_DEBUG_MCP_HTTP_ENABLED env var.
            if std::env::var("TYDE_DEBUG_MCP_HTTP_ENABLED").is_ok_and(|v| v == "true" || v == "1") {
                if let Err(err) = debug_mcp_http::start_debug_mcp_http_server(app.handle()) {
                    tracing::warn!("Debug MCP HTTP server failed to start: {err}");
                }
            }
            let driver_mcp_http_enabled = *app.state::<AppState>().driver_mcp_http_enabled.lock();
            if driver_mcp_http_enabled {
                if let Err(err) = driver_mcp_http::start_driver_mcp_http_server(app.handle()) {
                    tracing::warn!("Driver MCP HTTP server failed to start: {err}");
                }
            } else {
                tracing::info!("Driver MCP HTTP server disabled by app settings");
            }
            let remote_control_enabled = *app.state::<AppState>().remote_control_enabled.lock();
            if remote_control_enabled {
                match remote_control::RemoteControlServer::start(app.handle().clone()) {
                    Ok(server) => {
                        tracing::info!(
                            "Remote control server started on {}",
                            server.socket_path().display()
                        );
                        app.manage(server);
                    }
                    Err(err) => {
                        tracing::warn!("Remote control server failed to start: {err}");
                    }
                }
            } else {
                tracing::info!("Remote control server disabled by app settings");
            }
            if !headless {
                if let Some(window) = app.get_webview_window("main") {
                    let window_handle = window.clone();
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            if cfg!(target_os = "macos") {
                                api.prevent_close();
                                let _ = window_handle.hide();
                            }
                        }
                    });
                }
            } else {
                // In headless mode, destroy the default window created by tauri.conf.json.
                // On display-less Linux this prevents WebView initialization failures.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.destroy();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_initial_workspace,
            check_backend_dependencies,
            query_backend_usage,
            set_disabled_backends,
            install_backend_dependency,
            create_conversation,
            send_message,
            cancel_conversation,
            close_conversation,
            spawn_agent,
            send_agent_message,
            interrupt_agent,
            terminate_agent,
            get_agent,
            rename_agent,
            list_agents,
            wait_for_agent,
            agent_events_since,
            collect_agent_result,
            get_mcp_http_server_settings,
            set_mcp_http_server_enabled,
            get_driver_mcp_http_server_settings,
            set_driver_mcp_http_server_enabled,
            set_driver_mcp_http_server_autoload_enabled,
            set_mcp_control_enabled,
            get_remote_control_settings,
            set_remote_control_enabled,
            get_remote_tyde_server_status,
            install_remote_tyde_server,
            launch_remote_tyde_server,
            install_and_launch_remote_tyde_server,
            upgrade_remote_tyde_server,
            list_hosts,
            add_host,
            remove_host,
            update_host_label,
            update_host_enabled_backends,
            update_host_default_backend,
            get_host_for_workspace,
            submit_debug_ui_response,
            get_settings,
            list_models,
            list_sessions,
            resume_session,
            delete_session,
            get_session_id,
            list_session_records,
            rename_session,
            set_session_alias,
            list_profiles,
            switch_profile,
            get_module_schemas,
            update_settings,
            restart_subprocess,
            relaunch_conversation,
            list_active_conversations,
            shutdown_all_subprocesses,
            create_admin_subprocess,
            close_admin_subprocess,
            admin_list_sessions,
            admin_get_settings,
            admin_update_settings,
            admin_list_profiles,
            admin_switch_profile,
            admin_get_module_schemas,
            admin_delete_session,
            delete_session_record,
            list_projects,
            add_project,
            add_project_workbench,
            remove_project,
            rename_project,
            update_project_roots,
            discover_git_repos,
            git_current_branch,
            git_status,
            git_stage,
            git_unstage,
            git_commit,
            git_diff,
            git_diff_base_content,
            git_discard,
            git_worktree_add,
            git_worktree_remove,
            list_directory,
            read_file_content,
            sync_file_watch_paths,
            watch_workspace_dir,
            unwatch_workspace_dir,
            create_terminal,
            write_terminal,
            resize_terminal,
            close_terminal,
            submit_feedback,
            list_workflows,
            save_workflow,
            delete_workflow,
            run_shell_command,
            list_agent_definitions,
            save_agent_definition,
            delete_agent_definition,
            list_available_skills,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |app_handle, event| match event {
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            if headless && code.is_none() {
                // Headless mode has no windows/tray, so prevent implicit app exit.
                api.prevent_exit();
                return;
            }
            if let Some(rc) = app_handle.try_state::<remote_control::RemoteControlServer>() {
                rc.shutdown();
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(rc) = app_handle.try_state::<remote_control::RemoteControlServer>() {
                rc.shutdown();
            }
        }
        _ => {}
    });
}

fn initialize_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show_item = MenuItemBuilder::with_id("tray_show", "Show Tyde").build(app)?;
    let hide_item = MenuItemBuilder::with_id("tray_hide", "Hide").build(app)?;
    let quit_item = MenuItemBuilder::with_id("tray_quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&show_item, &hide_item, &quit_item])
        .build()?;

    let mut tray_builder = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray_show" => show_main_window(app),
            "tray_hide" => hide_main_window(app),
            "tray_quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        tray_builder = tray_builder.icon(icon);
    }

    let _ = tray_builder.build(app)?;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex as SyncMutex;
    use std::collections::{HashMap, HashSet};
    use std::process::Command;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use serde_json::json;

    fn test_app_state() -> AppState {
        let tmp_path = std::env::temp_dir().join(format!(
            "tyde-test-session-store-{}.json",
            uuid::Uuid::new_v4()
        ));
        AppState {
            manager: Mutex::new(ConversationManager::new()),
            admin: Mutex::new(AdminManager::new()),
            terminals: Mutex::new(TerminalManager::new()),
            file_watch: SyncMutex::new(None),
            agent_runtime: Arc::new(Mutex::new(AgentRuntime::new())),
            agent_runtime_notify: Arc::new(Notify::new()),
            session_store: Arc::new(SyncMutex::new(
                SessionStore::load(tmp_path).expect("failed to create test session store"),
            )),
            project_store: Arc::new(SyncMutex::new({
                let tmp_project_path = std::env::temp_dir().join(format!(
                    "tyde-test-project-store-{}.json",
                    uuid::Uuid::new_v4()
                ));
                ProjectStore::load(tmp_project_path).expect("failed to create test project store")
            })),
            host_store: SyncMutex::new({
                let tmp_host_path = std::env::temp_dir().join(format!(
                    "tyde-test-host-store-{}.json",
                    uuid::Uuid::new_v4()
                ));
                host::HostStore::load(tmp_host_path).expect("failed to create test host store")
            }),
            conversation_to_session: Arc::new(SyncMutex::new(HashMap::new())),
            remote_chat_senders: Arc::new(SyncMutex::new(HashMap::new())),
            mcp_http_enabled: SyncMutex::new(true),
            mcp_control_enabled: SyncMutex::new(true),
            driver_mcp_http_enabled: SyncMutex::new(false),
            driver_mcp_http_autoload: SyncMutex::new(false),
            remote_control_enabled: SyncMutex::new(false),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
            disabled_backends: SyncMutex::new(HashSet::new()),
            settings_watch: Mutex::new(HashMap::new()),
            dev_instances: SyncMutex::new(dev_instance::DevInstanceRegistry::new()),
            tyde_server_connections: SyncMutex::new(HashMap::new()),
            driver_mcp_http_env_override: false,
            skill_cleanups: SyncMutex::new(HashMap::new()),
        }
    }

    fn assert_transport_is_local(transport: BackendTransport) {
        assert!(
            matches!(transport, BackendTransport::Local),
            "expected local transport"
        );
    }

    fn assert_transport_is_remote(transport: BackendTransport, expected_host: &str) {
        match transport {
            BackendTransport::Ssh { host } => assert_eq!(host, expected_host),
            BackendTransport::Local => panic!("expected remote transport"),
        }
    }

    #[tokio::test]
    async fn wait_for_agent_waits_for_not_running() {
        let state = test_app_state();
        let conversation_id = 9001;
        let agent_id = {
            let mut runtime = state.agent_runtime.lock().await;
            let info = runtime.register_agent(
                conversation_id,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "test".into(),
                None,
            );
            info.agent_id
        };

        let wait_fut = wait_for_agent_internal(&state, WaitForAgentRequest { agent_id });
        let notifier = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            {
                let mut runtime = state.agent_runtime.lock().await;
                let changed = runtime.record_chat_event(
                    conversation_id,
                    &json!({ "kind": "TypingStatusChanged", "data": false }),
                );
                assert!(changed);
            }
            state.agent_runtime_notify.notify_waiters();
        };
        let (result, _) = tokio::join!(wait_fut, notifier);
        let agent = result.expect("wait_for_agent should return once agent stops running");
        assert!(!agent.is_running);
    }

    #[tokio::test]
    async fn await_agents_rejects_self_wait() {
        let state = test_app_state();
        let caller_agent_id = {
            let mut runtime = state.agent_runtime.lock().await;
            runtime
                .register_agent(
                    9101,
                    vec!["/tmp".into()],
                    "tycode".into(),
                    None,
                    "parent".into(),
                    None,
                )
                .agent_id
        };

        let result = await_agents_internal(
            &state,
            AwaitAgentsRequest {
                agent_ids: vec![caller_agent_id.clone()],
                timeout_ms: Some(10),
            },
            Some(&caller_agent_id),
        )
        .await;
        let err = match result {
            Ok(_) => panic!("awaiting self should fail"),
            Err(err) => err,
        };

        assert!(
            err.contains("cannot await itself"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn await_agents_rejects_non_child_watch_targets() {
        let state = test_app_state();
        let (caller_agent_id, unrelated_agent_id) = {
            let mut runtime = state.agent_runtime.lock().await;
            let caller = runtime.register_agent(
                9201,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "parent".into(),
                None,
            );
            let unrelated = runtime.register_agent(
                9202,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "other".into(),
                None,
            );
            (caller.agent_id, unrelated.agent_id)
        };

        let result = await_agents_internal(
            &state,
            AwaitAgentsRequest {
                agent_ids: vec![unrelated_agent_id.clone()],
                timeout_ms: Some(10),
            },
            Some(&caller_agent_id),
        )
        .await;
        let err = match result {
            Ok(_) => panic!("awaiting non-child should fail"),
            Err(err) => err,
        };

        assert!(
            err.contains("is not a direct child"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn list_child_agents_filters_by_parent() {
        let state = test_app_state();
        let (parent_id, child_a_id, child_b_id) = {
            let mut runtime = state.agent_runtime.lock().await;
            let parent = runtime.register_agent(
                9301,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "parent".into(),
                None,
            );
            let child_a = runtime.register_agent(
                9302,
                vec!["/tmp".into()],
                "tycode".into(),
                Some(parent.agent_id.clone()),
                "child-a".into(),
                None,
            );
            let child_b = runtime.register_agent(
                9303,
                vec!["/tmp".into()],
                "tycode".into(),
                Some(parent.agent_id.clone()),
                "child-b".into(),
                None,
            );
            let _other = runtime.register_agent(
                9304,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "other".into(),
                None,
            );
            (parent.agent_id, child_a.agent_id, child_b.agent_id)
        };

        let children = list_child_agents_internal(&state, &parent_id)
            .await
            .expect("list children");
        let child_ids = children
            .iter()
            .map(|a| a.agent_id.as_str())
            .collect::<HashSet<_>>();

        assert_eq!(children.len(), 2);
        assert!(child_ids.contains(child_a_id.as_str()));
        assert!(child_ids.contains(child_b_id.as_str()));
    }

    #[test]
    fn login_shell_returns_nonempty_path() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let output = Command::new(&shell)
            .args(["-l", "-c", "echo $PATH"])
            .output()
            .expect("failed to spawn login shell");

        assert!(output.status.success(), "login shell exited with failure");
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(!path.is_empty(), "login shell returned empty PATH");
        assert!(
            path.contains("/usr/bin"),
            "resolved PATH missing /usr/bin: {path}"
        );
    }

    #[test]
    fn should_upgrade_remote_tyde_when_not_installed() {
        assert!(should_upgrade_remote_tyde("0.7.0", None, false));
    }

    #[test]
    fn should_upgrade_remote_tyde_only_when_remote_is_older() {
        assert!(should_upgrade_remote_tyde("0.7.0", Some("0.6.9"), true));
        assert!(!should_upgrade_remote_tyde("0.7.0", Some("0.7.0"), true));
        assert!(!should_upgrade_remote_tyde("0.7.0", Some("0.8.0"), true));
    }

    #[test]
    fn should_upgrade_remote_tyde_falls_back_for_non_numeric_versions() {
        assert!(should_upgrade_remote_tyde("0.7.0", Some("dev-build"), true));
        assert!(!should_upgrade_remote_tyde("0.7.0", Some("0.7.0"), true));
    }

    #[test]
    fn invalid_shell_does_not_panic() {
        let output = Command::new("/nonexistent/shell")
            .args(["-l", "-c", "echo $PATH"])
            .output();

        assert!(output.is_err(), "expected error for nonexistent shell");
    }

    #[test]
    fn login_shell_path_contains_no_extraneous_output() {
        // Some shells print motd/greeting to stdout during login.
        // Verify that `echo $PATH` produces a single line with colon-separated paths.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let output = Command::new(&shell)
            .args(["-l", "-c", "echo $PATH"])
            .output()
            .expect("failed to spawn login shell");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        let lines: Vec<&str> = trimmed.lines().collect();

        // The last line should be the PATH value
        let path_line = lines.last().expect("no output from login shell");
        assert!(
            path_line.contains('/'),
            "last line doesn't look like a PATH: {path_line}"
        );
        // PATH entries are colon-separated
        assert!(
            path_line.contains(':'),
            "PATH has no colon separators: {path_line}"
        );
    }

    #[test]
    fn launch_target_for_backend_routes_local_and_remote() {
        let local_codex = launch_target_for_backend(BackendKind::Codex, None, None).unwrap();
        assert_transport_is_local(local_codex.transport);
        assert!(local_codex.executable_path.is_empty());

        let remote_codex =
            launch_target_for_backend(BackendKind::Codex, Some("dev.example.com".into()), None)
                .unwrap();
        assert_transport_is_remote(remote_codex.transport, "dev.example.com");
        assert!(remote_codex.executable_path.is_empty());

        let local_tycode = launch_target_for_backend(
            BackendKind::Tycode,
            None,
            Some("/tmp/tycode-subprocess".into()),
        )
        .unwrap();
        assert_transport_is_local(local_tycode.transport);
        assert_eq!(local_tycode.executable_path, "/tmp/tycode-subprocess");

        let remote_tycode = launch_target_for_backend(
            BackendKind::Tycode,
            Some("ssh-host".into()),
            Some("/opt/tycode-subprocess".into()),
        )
        .unwrap();
        assert_transport_is_remote(remote_tycode.transport, "ssh-host");
        assert_eq!(remote_tycode.executable_path, "/opt/tycode-subprocess");
    }

    #[test]
    fn launch_target_for_backend_requires_tycode_path() {
        let err = launch_target_for_backend(BackendKind::Tycode, None, None).unwrap_err();
        assert!(
            err.contains("Missing tycode executable path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn usage_transport_for_host_id_maps_local_and_remote_hosts() {
        let state = test_app_state();
        let remote_id = {
            let mut store = state.host_store.lock();
            store
                .add(
                    "Remote".into(),
                    "alice@remote.example.com".into(),
                    host::RemoteKind::SshPipe,
                )
                .unwrap()
                .id
        };

        let local_transport = usage_transport_for_host_id(&state, Some("local")).unwrap();
        assert_transport_is_local(local_transport);

        let remote_transport = usage_transport_for_host_id(&state, Some(&remote_id)).unwrap();
        assert_transport_is_remote(remote_transport, "alice@remote.example.com");

        let implicit_local = usage_transport_for_host_id(&state, None).unwrap();
        assert_transport_is_local(implicit_local);
    }

    #[test]
    fn usage_transport_for_host_id_errors_for_unknown_host() {
        let state = test_app_state();
        let err = usage_transport_for_host_id(&state, Some("missing-host")).unwrap_err();
        assert!(err.contains("Host 'missing-host' not found"));
    }
}

fn toggle_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        match window.is_visible() {
            Ok(true) => {
                let _ = window.hide();
            }
            Ok(false) => show_main_window(app),
            Err(_) => show_main_window(app),
        }
    }
}
