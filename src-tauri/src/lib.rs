mod acp;
mod admin;
mod agent_mcp_http;
mod agent_runtime;
mod backend;
mod claude;
mod codex;
mod conversation;
mod debug_mcp_http;
mod dev_instance;
mod driver_mcp_http;
mod file_service;
mod file_watch;
mod git_service;
pub mod host;
mod kiro;
mod remote;
mod session_store;
mod steering;
mod subprocess;
mod terminal;
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
use tokio::fs as tokio_fs;
use tokio::sync::{mpsc, watch, Mutex, Notify};

use crate::admin::AdminManager;
use crate::agent_runtime::{AgentEventBatch, AgentInfo, AgentRuntime, CollectedAgentResult};
use crate::backend::{
    BackendKind, BackendSession, SessionCommand, StartupMcpServer, StartupMcpTransport,
};
use crate::claude::{SubAgentEmitter, SubAgentHandle};
use crate::conversation::ConversationManager;
use crate::file_service::{FileContent, FileEntry};
use crate::file_watch::FileWatchManager;
use crate::git_service::GitFileStatus;
use crate::remote::{
    connect_remote_with_progress, parse_remote_path, parse_remote_workspace_roots,
    validate_remote_cli, SUBPROCESS_CRATE_NAME, SUBPROCESS_GIT_REPO, SUBPROCESS_VERSION,
};
use crate::session_store::SessionStore;
use crate::subprocess::ImageAttachment;
use crate::terminal::TerminalManager;

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
    parent_agent_id: Option<u64>,
    /// Lazily populated when `parent_agent_id` is `None` and the first
    /// sub-agent is spawned. Subsequent sub-agents reuse this value.
    lazy_parent_agent_id: Mutex<Option<u64>>,
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
    async fn resolve_parent_agent_id(&self) -> Option<u64> {
        if let Some(id) = self.parent_agent_id {
            return Some(id);
        }
        let mut lazy = self.lazy_parent_agent_id.lock().await;
        if let Some(id) = *lazy {
            return Some(id);
        }
        // Register the parent conversation in the runtime so sub-agents
        // can reference it as their parent.
        let mut runtime = self.agent_runtime.lock().await;
        let info = runtime.register_agent(
            self.parent_conversation_id,
            self.workspace_roots.clone(),
            self.backend_kind.clone(),
            None,
            "Conversation".to_string(),
        );
        let id = info.agent_id;
        *lazy = Some(id);
        Some(id)
    }

    async fn emit_agent_changed(&self, agent_id: u64) {
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
                    parent_agent_id,
                    display_name,
                );
                info.agent_type = if agent_type.is_empty() {
                    None
                } else {
                    Some(agent_type)
                };
                runtime.update_agent_type(info.agent_id, info.agent_type.clone());
                info
            };
            self.agent_runtime_notify.notify_waiters();
            self.emit_agent_changed(agent_info.agent_id).await;

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
                    "agent_id": agent_info.agent_id,
                    "workspace_roots": self.workspace_roots,
                    "backend_kind": &self.backend_kind,
                    "name": &agent_info.name,
                    "agent_type": &agent_info.agent_type,
                    "parent_agent_id": parent_agent_id,
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
        agent_id: u64,
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
                let current_info = runtime.get_agent(agent_id);
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
    host_store: SyncMutex<host::HostStore>,
    conversation_to_session: Arc<SyncMutex<HashMap<u64, String>>>,
    mcp_http_enabled: SyncMutex<bool>,
    mcp_control_enabled: SyncMutex<bool>,
    driver_mcp_http_enabled: SyncMutex<bool>,
    driver_mcp_http_autoload: SyncMutex<bool>,
    driver_mcp_http_env_override: bool,
    debug_event_log: SyncMutex<DebugEventLog>,
    debug_ui_pending:
        SyncMutex<HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>>>,
    debug_ui_request_seq: AtomicU64,
    disabled_backends: SyncMutex<HashSet<String>>,
    settings_watch: Mutex<HashMap<u64, watch::Sender<Value>>>,
    dev_instance: SyncMutex<Option<dev_instance::DevInstance>>,
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
    pub(crate) agent_id: u64,
    pub(crate) conversation_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SpawnAgentRequest {
    pub(crate) workspace_roots: Vec<String>,
    pub(crate) prompt: String,
    pub(crate) backend_kind: Option<String>,
    pub(crate) parent_agent_id: Option<u64>,
    pub(crate) name: String,
    pub(crate) ephemeral: Option<bool>,
    /// Images to attach to the initial message sent to the agent.
    #[serde(skip)]
    pub(crate) images: Option<Vec<ImageAttachment>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SendAgentMessageRequest {
    pub(crate) agent_id: u64,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgentIdRequest {
    pub(crate) agent_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WaitForAgentRequest {
    pub(crate) agent_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgentEventsSinceRequest {
    pub(crate) since_seq: Option<u64>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AwaitAgentsRequest {
    pub(crate) agent_ids: Vec<u64>,
    pub(crate) timeout_ms: Option<u64>,
}

/// Simplified agent result returned by the push-oriented MCP tools.
#[derive(Serialize, Clone)]
pub(crate) struct AgentResult {
    pub(crate) agent_id: u64,
    pub(crate) is_running: bool,
    pub(crate) message: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) summary: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct AwaitAgentsResponse {
    pub(crate) ready: Vec<AgentResult>,
    pub(crate) still_running: Vec<u64>,
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

fn resolve_tycode_sessions_dir() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".tycode").join("sessions"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(profile).join(".tycode").join("sessions"));
    }
    Err("Could not determine home directory for session export".to_string())
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
        settings.driver_mcp_http_enabled = val == "true" || val == "1";
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
    startup_mcp_servers_for_agent(state, include_agent_control, None, workspace_roots)
}

fn startup_mcp_servers_for_agent(
    state: &AppState,
    include_agent_control: bool,
    caller_agent_id: Option<u64>,
    _workspace_roots: &[String],
) -> Result<Vec<StartupMcpServer>, String> {
    let mut servers = Vec::new();

    if include_agent_control {
        let server_enabled = *state.mcp_http_enabled.lock();
        if !server_enabled {
            return Err("Tyde MCP control server must be enabled for Bridge chats".to_string());
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

    Ok(servers)
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
    app.emit(
        "tyde-delete-workbench",
        &DeleteWorkbenchEventPayload { workspace_path },
    )
    .map_err(|err| format!("Failed to emit delete workbench event: {err:?}"))?;
    Ok(())
}

fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn resolve_requested_backend_kind(
    state: &AppState,
    backend_kind: Option<String>,
    workspace_roots: &[String],
) -> Result<BackendKind, String> {
    let host = resolve_host_for_roots(state, workspace_roots)?;

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

fn resolve_host_for_roots(
    state: &AppState,
    workspace_roots: &[String],
) -> Result<host::Host, String> {
    let store = state.host_store.lock();
    let first_root = workspace_roots.first().map(|s| s.as_str()).unwrap_or("");
    if let Some(remote) = parse_remote_path(first_root) {
        for h in store.list() {
            if !h.is_local && h.hostname == remote.host {
                return Ok(h);
            }
        }
        return Err(format!(
            "Remote host '{}' is not registered. Open Settings → Hosts to add it.",
            remote.host
        ));
    }
    store
        .get("local")
        .cloned()
        .ok_or_else(|| "Local host not found in host store".to_string())
}

async fn resolve_backend_executable_path(
    app: &tauri::AppHandle,
    workspace_roots: &[String],
    backend_kind: BackendKind,
) -> Result<String, String> {
    match backend_kind {
        BackendKind::Tycode => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => connect_remote_with_progress(app, host).await,
                None => subprocess_path(),
            }
        }
        BackendKind::Codex => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "codex").await?;
                    Ok(host.clone())
                }
                None => Ok(String::new()),
            }
        }
        BackendKind::Claude => {
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "claude").await?;
                    Ok(host.clone())
                }
                None => Ok(String::new()),
            }
        }
        BackendKind::Kiro => {
            // parse_remote_workspace_roots errors on mixed local+SSH roots.
            // For admin sessions with mixed bridge projects, fall through to
            // local so list_sessions can query both local and SSH roots.
            match parse_remote_workspace_roots(workspace_roots) {
                Ok(Some((host, _))) => {
                    validate_remote_cli(app, &host, "kiro-cli-chat").await?;
                    Ok(host.clone())
                }
                _ => Ok(String::new()),
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
    }
}

#[tauri::command]
async fn query_backend_usage(
    state: tauri::State<'_, AppState>,
    backend_kind: String,
    host_id: Option<String>,
) -> Result<Value, String> {
    let ssh_host = if let Some(id) = host_id.as_deref() {
        let store = state.host_store.lock();
        let host = store
            .get(id)
            .ok_or_else(|| format!("Host '{id}' not found"))?;
        if host.is_local {
            None
        } else {
            Some(host.hostname.clone())
        }
    } else {
        None
    };
    usage::query_backend_usage_for_host(&backend_kind, ssh_host.as_deref()).await
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

#[tauri::command]
async fn install_backend_dependency(backend_kind: String) -> Result<(), String> {
    match backend_kind.as_str() {
        "tycode" => install_tycode_subprocess().await,
        "codex" => install_codex().await,
        "claude" => install_claude_code().await,
        "kiro" => install_kiro().await,
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
    conversation_mode: Option<String>,
) -> Result<CreateConversationResponse, String> {
    let backend_kind = resolve_requested_backend_kind(&state, backend_kind, &workspace_roots)?;
    let ephemeral = ephemeral.unwrap_or(false);
    let include_agent_control = matches!(
        conversation_mode
            .as_deref()
            .map(|raw| raw.trim().to_ascii_lowercase())
            .as_deref(),
        Some("bridge")
    );

    // For Bridge conversations, reserve an agent_id upfront so we can embed it
    // in the MCP startup config. The MCP server uses this to auto-inject
    // parent_agent_id when spawning sub-agents.
    let reserved_agent_id = if include_agent_control {
        let mut runtime = state.agent_runtime.lock().await;
        Some(runtime.reserve_agent_id())
    } else {
        None
    };

    let resolved_path =
        resolve_backend_executable_path(&app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers = startup_mcp_servers_for_agent(
        state.inner(),
        include_agent_control,
        reserved_agent_id,
        &workspace_roots,
    )?;
    let steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
        steering.as_deref(),
    )
    .await?;

    let id = {
        let mut mgr = state.manager.lock().await;
        mgr.create_conversation(session, &workspace_roots)
    };

    // For Bridge conversations, complete the agent registration using the
    // reserved ID so it appears in the hierarchy and sub-agents can reference it.
    if let Some(agent_id) = reserved_agent_id {
        let mut runtime = state.agent_runtime.lock().await;
        let info = runtime.register_agent_with_id(
            agent_id,
            id,
            workspace_roots.clone(),
            backend_kind.as_str().to_string(),
            None,
            "Bridge".to_string(),
        );
        drop(runtime);
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(&app, &state, info.agent_id).await;
    }

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
            "agent_id": reserved_agent_id,
            "workspace_roots": workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": if reserved_agent_id.is_some() { "Bridge" } else { "Conversation" },
            "parent_agent_id": null,
        }
    });

    let (settings_tx, _) = watch::channel(Value::Null);
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.insert(id, settings_tx.clone());
    }

    // Create session store record
    let workspace_root = workspace_roots.first().map(|s| s.as_str());
    let session_record = {
        let mut store = state.session_store.lock();
        store.create(backend_kind.as_str(), workspace_root)?
    };
    let tyde_session_id = session_record.id.clone();
    {
        let mut map = state.conversation_to_session.lock();
        map.insert(id, tyde_session_id.clone());
    }

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
            let info = {
                agent_runtime
                    .lock()
                    .await
                    .get_agent_by_conversation(conversation_id)
            };
            if let Some(info) = info {
                let _ = app.emit("agent-changed", &info);
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
    }

    // Flush any pending session store writes when the event stream ends.
    if let Err(err) = session_store.lock().flush() {
        tracing::error!("Failed to flush session store at conversation end: {err}");
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

async fn emit_agent_changed(app: &tauri::AppHandle, state: &AppState, agent_id: u64) {
    let info = { state.agent_runtime.lock().await.get_agent(agent_id) };
    if let Some(info) = info {
        let _ = app.emit("agent-changed", &info);
    }
}

async fn emit_agent_changed_for_conversation(
    app: &tauri::AppHandle,
    state: &AppState,
    conversation_id: u64,
) {
    let info = {
        state
            .agent_runtime
            .lock()
            .await
            .get_agent_by_conversation(conversation_id)
    };
    if let Some(info) = info {
        let _ = app.emit("agent-changed", &info);
    }
}

async fn execute_conversation_command(
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
    let session = {
        let mut mgr = state.manager.lock().await;
        mgr.remove(conversation_id)
            .ok_or("Conversation not found")?
    };
    session.shutdown().await;
    {
        let mut watchers = state.settings_watch.lock().await;
        watchers.remove(&conversation_id);
    }
    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_conversation_closed(conversation_id, Some("Conversation closed".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed_for_conversation(&app, &state, conversation_id).await;
    }
    // Clean up runtime map (store record persists)
    state
        .conversation_to_session
        .lock()
        .remove(&conversation_id);
    Ok(())
}

pub(crate) async fn spawn_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: SpawnAgentRequest,
) -> Result<SpawnAgentResponse, String> {
    let SpawnAgentRequest {
        workspace_roots,
        prompt,
        backend_kind,
        parent_agent_id,
        name,
        ephemeral,
        images,
    } = request;

    if workspace_roots.iter().all(|root| root.trim().is_empty()) {
        return Err("spawn_agent requires at least one workspace root".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("spawn_agent requires a non-empty prompt".to_string());
    }

    if let Some(parent_id) = parent_agent_id {
        let exists = {
            let runtime = state.agent_runtime.lock().await;
            runtime.has_agent(parent_id)
        };
        if !exists {
            return Err(format!("Parent agent {parent_id} was not found"));
        }
    }

    let backend_kind = resolve_requested_backend_kind(state, backend_kind, &workspace_roots)?;
    let ephemeral = ephemeral.unwrap_or(false);
    let resolved_path =
        resolve_backend_executable_path(app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers = startup_mcp_servers_for_new_sessions(state, false, &workspace_roots)?;
    let steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
        steering.as_deref(),
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

    let info = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.register_agent(
            conversation_id,
            workspace_roots.clone(),
            backend_kind.as_str().to_string(),
            parent_agent_id,
            display_name.clone(),
        )
    };
    state.agent_runtime_notify.notify_waiters();
    emit_agent_changed(app, state, info.agent_id).await;

    // Create session store record for this agent
    let tyde_session_id = {
        let workspace_root = workspace_roots.first().map(|s| s.as_str());
        let record = state
            .session_store
            .lock()
            .create(backend_kind.as_str(), workspace_root)?;
        record.id
    };
    // Set parent_id if this agent has a parent
    if let Some(parent_runtime_agent_id) = parent_agent_id {
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
                parent_agent_id: Some(info.agent_id),
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
            "agent_id": info.agent_id,
            "workspace_roots": workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": info.name,
            "parent_agent_id": parent_agent_id,
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

    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(agent_id)
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
    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    // Cascade interrupt to child agents first
    let child_ids: Vec<u64> = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .children_of(agent_id)
            .iter()
            .filter(|c| c.is_running)
            .map(|c| c.agent_id)
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
            emit_agent_changed(app, state, agent_id).await;
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
        runtime.mark_agent_running(agent_id, Some("Cancelling...".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
        emit_agent_changed(app, state, agent_id).await;
    }

    Ok(())
}

pub(crate) async fn terminate_agent_internal(
    app: &tauri::AppHandle,
    state: &AppState,
    request: AgentIdRequest,
) -> Result<(), String> {
    let agent_id = request.agent_id;
    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(agent_id)
            .ok_or(format!("Agent {agent_id} not found"))?
    };

    // Cascade termination to child agents first
    let child_ids: Vec<u64> = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .children_of(agent_id)
            .iter()
            .filter(|c| c.is_running)
            .map(|c| c.agent_id)
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
        emit_agent_changed(app, state, agent_id).await;
    }

    // Clean up conversation_to_session map (store record persists)
    state
        .conversation_to_session
        .lock()
        .remove(&conversation_id);

    Ok(())
}

pub(crate) async fn get_agent_internal(
    state: &AppState,
    request: AgentIdRequest,
) -> Result<Option<AgentInfo>, String> {
    let runtime = state.agent_runtime.lock().await;
    Ok(runtime.get_agent(request.agent_id))
}

pub(crate) async fn list_agents_internal(state: &AppState) -> Result<Vec<AgentInfo>, String> {
    let runtime = state.agent_runtime.lock().await;
    Ok(runtime.list_agents())
}

pub(crate) async fn wait_for_agent_internal(
    state: &AppState,
    request: WaitForAgentRequest,
) -> Result<AgentInfo, String> {
    let agent_id = request.agent_id;

    loop {
        // Create the Notified future BEFORE checking state to avoid missing
        // a notification that fires between the state check and the await.
        let notified = state.agent_runtime_notify.notified();

        let current = {
            let runtime = state.agent_runtime.lock().await;
            runtime
                .get_agent(agent_id)
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
    let runtime = state.agent_runtime.lock().await;
    runtime.collect_result(request.agent_id)
}

fn agent_result_from_info(info: &AgentInfo) -> AgentResult {
    AgentResult {
        agent_id: info.agent_id,
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
    question: String,
) -> Result<String, String> {
    // 1. Take a screenshot via the debug MCP proxy.
    let screenshot_result = dev_instance::proxy_debug_tool_call(
        state,
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
    let project_dir =
        dev_instance::dev_instance_project_dir(state).ok_or("No dev instance running")?;

    let request = SpawnAgentRequest {
        workspace_roots: vec![project_dir],
        prompt,
        backend_kind: None,
        parent_agent_id: None,
        name: "__internal_query_screenshot__".to_string(),
        ephemeral: Some(true),
        images: Some(vec![image]),
    };

    let spawn_resp = spawn_agent_internal(app, state, request).await?;
    let agent_id = spawn_resp.agent_id;

    let wait_result = wait_for_agent_internal(state, WaitForAgentRequest { agent_id }).await;

    // Collect result and terminate regardless of wait outcome.
    let result = collect_agent_result_internal(state, AgentIdRequest { agent_id }).await;

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
) -> Result<AwaitAgentsResponse, String> {
    let idle_timeout = request
        .timeout_ms
        .unwrap_or(60_000)
        .clamp(1, 30 * 60 * 1000);
    let idle_duration = tokio::time::Duration::from_millis(idle_timeout);
    let max_wall = idle_duration.saturating_mul(10);
    let wall_deadline = tokio::time::Instant::now() + max_wall;
    let mut idle_deadline = tokio::time::Instant::now() + idle_duration;
    let mut last_updated_at_ms: Option<u64> = None;

    loop {
        // Create the Notified future BEFORE checking state to avoid missing
        // a notification that fires between the state check and the await.
        let notified = state.agent_runtime_notify.notified();

        let (ready, still_running, newest_updated_at) = {
            let runtime = state.agent_runtime.lock().await;
            // Validate all requested IDs exist.
            for &id in &request.agent_ids {
                if !runtime.has_agent(id) {
                    return Err(format!("Agent {id} not found"));
                }
            }
            let watch_ids = request.agent_ids.clone();

            if watch_ids.is_empty() {
                return Err("No agents to watch".to_string());
            }

            let mut ready = Vec::new();
            let mut still_running = Vec::new();
            let mut newest: u64 = 0;
            for &id in &watch_ids {
                let info = runtime
                    .get_agent(id)
                    .ok_or(format!("Agent {id} not found"))?;
                if info.updated_at_ms > newest {
                    newest = info.updated_at_ms;
                }
                if !info.is_running {
                    ready.push(agent_result_from_info(&info));
                } else {
                    still_running.push(id);
                }
            }
            (ready, still_running, newest)
        };

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
        match tokio::time::timeout(remaining, notified).await {
            Ok(_) => {}
            Err(_) => return Err("Timed out waiting for agents".to_string()),
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
    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime
            .conversation_id_for_agent(agent_id)
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
        emit_agent_changed(app, state, agent_id).await;
    }

    // Clean up conversation_to_session map (store record persists)
    state
        .conversation_to_session
        .lock()
        .remove(&conversation_id);

    let runtime = state.agent_runtime.lock().await;
    let info = runtime
        .get_agent(agent_id)
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
    parent_agent_id: Option<u64>,
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
            name,
            ephemeral,
            images: None,
        },
    )
    .await
}

#[tauri::command]
async fn send_agent_message(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    agent_id: u64,
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
    agent_id: u64,
) -> Result<(), String> {
    interrupt_agent_internal(&app, state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn terminate_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    agent_id: u64,
) -> Result<(), String> {
    terminate_agent_internal(&app, state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn get_agent(
    state: tauri::State<'_, AppState>,
    agent_id: u64,
) -> Result<Option<AgentInfo>, String> {
    get_agent_internal(state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn rename_agent(
    state: tauri::State<'_, AppState>,
    agent_id: u64,
    name: String,
) -> Result<(), String> {
    let conversation_id = {
        let runtime = state.agent_runtime.lock().await;
        runtime.conversation_id_for_agent(agent_id)
    };
    {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.rename_agent(agent_id, name.clone());
    }
    // Update session store alias
    if let Some(cid) = conversation_id {
        let tyde_session_id = state.conversation_to_session.lock().get(&cid).cloned();
        if let Some(sid) = tyde_session_id {
            state.session_store.lock().set_alias(&sid, &name)?;
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
    agent_id: u64,
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
    agent_id: u64,
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

#[tauri::command]
fn list_hosts(state: tauri::State<'_, AppState>) -> Result<Vec<host::Host>, String> {
    let store = state.host_store.lock();
    Ok(store.list())
}

#[tauri::command]
fn add_host(
    state: tauri::State<'_, AppState>,
    label: String,
    hostname: String,
) -> Result<host::Host, String> {
    let mut store = state.host_store.lock();
    store.add(label, hostname)
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
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::ResumeSession {
            session_id: session_id.clone(),
        },
    )
    .await?;

    // Deduplicate session store records: create_conversation eagerly created a
    // new record, but resume reuses an existing backend session. Find the
    // original record and point conversation_to_session at it, then delete the
    // duplicate.
    let backend_kind = {
        let mgr = state.manager.lock().await;
        mgr.backend_kind(conversation_id)
            .map(|k| k.as_str().to_string())
    };
    if let Some(bk) = backend_kind {
        let duplicate_tyde_id = state
            .conversation_to_session
            .lock()
            .get(&conversation_id)
            .cloned();
        let mut store = state.session_store.lock();
        if let Some(existing) = store.get_by_backend_session(&bk, &session_id) {
            let existing_id = existing.id.clone();
            // Only deduplicate if the conversation is currently mapped to a different record
            if duplicate_tyde_id.as_deref() != Some(&existing_id) {
                // Point the conversation at the original record
                state
                    .conversation_to_session
                    .lock()
                    .insert(conversation_id, existing_id);
                // Delete the duplicate record created by create_conversation
                if let Some(dup_id) = duplicate_tyde_id {
                    store.delete(&dup_id)?;
                }
            }
        } else if let Some(ref dup_id) = duplicate_tyde_id {
            // No existing record found — this is a first-time resume. Set the
            // backend_session_id on the record that create_conversation made.
            store.set_backend_session_id(dup_id, &session_id)?;
        }
    }

    Ok(())
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
    let store = state.session_store.lock();
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
    let path = resolve_backend_executable_path(&app, &workspace_roots, backend_kind).await?;
    let (session, rx) = BackendSession::spawn_admin(backend_kind, &path, &workspace_roots).await?;

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
fn list_session_records(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<session_store::SessionRecord>, String> {
    let store = state.session_store.lock();
    Ok(store.list())
}

#[tauri::command]
fn rename_session(
    state: tauri::State<'_, AppState>,
    id: String,
    name: String,
) -> Result<(), String> {
    let mut store = state.session_store.lock();
    store.set_user_alias(&id, &name)
}

#[tauri::command]
async fn export_session_json(session_id: String) -> Result<String, String> {
    if !is_valid_session_id(&session_id) {
        return Err("Invalid session id".to_string());
    }

    let sessions_dir = resolve_tycode_sessions_dir()?;
    let file_path = sessions_dir.join(format!("{session_id}.json"));
    tokio_fs::read_to_string(&file_path)
        .await
        .map_err(|e| format!("Failed to export session JSON: {e}"))
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

    let resolved_path =
        resolve_backend_executable_path(&app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers =
        startup_mcp_servers_for_new_sessions(state.inner(), false, &workspace_roots)?;
    let steering = steering::read_steering_from_roots(&workspace_roots).await?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        false,
        &startup_mcp_servers,
        steering.as_deref(),
    )
    .await?;

    let registration = serde_json::json!({
        "kind": "ConversationRegistered",
        "data": {
            "agent_id": null,
            "workspace_roots": &workspace_roots,
            "backend_kind": backend_kind.as_str(),
            "name": "Conversation",
            "parent_agent_id": null,
        }
    });

    {
        let mut mgr = state.manager.lock().await;
        mgr.insert(conversation_id, session, workspace_roots);
    }

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

pub fn run() {
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
    #[cfg(target_os = "linux")]
    if detect_system_dark_mode() {
        std::env::set_var("GTK_THEME", "Adwaita:dark");
    }
    let driver_mcp_http_env_override = std::env::var("TYDE_DRIVER_MCP_HTTP_ENABLED").is_ok();
    let mut app_settings = load_app_settings();
    if !app_settings.driver_mcp_http_enabled {
        app_settings.driver_mcp_http_autoload = false;
    }

    tauri::Builder::default()
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
            host_store: SyncMutex::new({
                let path = resolve_tyde_app_settings_path()
                    .map(|p| p.parent().unwrap().join("hosts.json"))
                    .unwrap_or_else(|_| PathBuf::from("hosts.json"));
                host::HostStore::load(path).expect("failed to load host store")
            }),
            conversation_to_session: Arc::new(SyncMutex::new(HashMap::new())),
            mcp_http_enabled: SyncMutex::new(app_settings.mcp_http_enabled),
            mcp_control_enabled: SyncMutex::new(app_settings.mcp_control_enabled),
            driver_mcp_http_enabled: SyncMutex::new(app_settings.driver_mcp_http_enabled),
            driver_mcp_http_autoload: SyncMutex::new(app_settings.driver_mcp_http_autoload),
            driver_mcp_http_env_override,
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
            disabled_backends: SyncMutex::new(HashSet::new()),
            settings_watch: Mutex::new(HashMap::new()),
            dev_instance: SyncMutex::new(None),
        })
        .setup(|app| {
            initialize_tray(app)?;
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
            if let Some(window) = app.get_webview_window("main") {
                let window_handle = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        if cfg!(target_os = "macos") {
                            api.prevent_close();
                            let _ = window_handle.hide();
                        }
                        // On Linux/Windows, let the close proceed normally and exit the app
                    }
                });
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
            list_profiles,
            switch_profile,
            get_module_schemas,
            update_settings,
            export_session_json,
            restart_subprocess,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
            host_store: SyncMutex::new({
                let tmp_host_path = std::env::temp_dir().join(format!(
                    "tyde-test-host-store-{}.json",
                    uuid::Uuid::new_v4()
                ));
                host::HostStore::load(tmp_host_path).expect("failed to create test host store")
            }),
            conversation_to_session: Arc::new(SyncMutex::new(HashMap::new())),
            mcp_http_enabled: SyncMutex::new(true),
            mcp_control_enabled: SyncMutex::new(true),
            driver_mcp_http_enabled: SyncMutex::new(false),
            driver_mcp_http_autoload: SyncMutex::new(false),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
            disabled_backends: SyncMutex::new(HashSet::new()),
            settings_watch: Mutex::new(HashMap::new()),
            dev_instance: SyncMutex::new(None),
            driver_mcp_http_env_override: false,
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
