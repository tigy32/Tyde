mod admin;
mod agent_mcp_http;
mod agent_runtime;
mod backend;
mod claude;
mod codex;
mod conversation;
mod debug_mcp_http;
mod file_service;
mod file_watch;
mod git_service;
mod kiro;
mod acp;
mod remote;
mod subprocess;
mod terminal;

use parking_lot::Mutex as SyncMutex;
use std::collections::{HashMap, VecDeque};
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
use tokio::sync::{mpsc, Mutex, Notify};

use crate::admin::AdminManager;
use crate::agent_runtime::{
    AgentEventBatch, AgentInfo, AgentRuntime, AgentStatus, CollectedAgentResult,
};
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
    validate_remote_cli,
};
use crate::subprocess::ImageAttachment;
use crate::terminal::TerminalManager;

/// Implements SubAgentEmitter for Claude Code sessions. Registers sub-agents
/// in the Tyde AgentRuntime and creates per-sub-agent event forwarding.
///
/// For non-bridge conversations, `parent_agent_id` starts as `None`. The first
/// time a sub-agent is spawned, the parent conversation is lazily registered in
/// the AgentRuntime so that the parent-child hierarchy is visible in the UI.
struct ClaudeSubAgentEmitter {
    app: tauri::AppHandle,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    parent_agent_id: Option<u64>,
    /// Lazily populated when `parent_agent_id` is `None` and the first
    /// sub-agent is spawned. Subsequent sub-agents reuse this value.
    lazy_parent_agent_id: Mutex<Option<u64>>,
    parent_conversation_id: u64,
    workspace_roots: Vec<String>,
}

impl ClaudeSubAgentEmitter {
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
            "claude".to_string(),
            None,
            "Conversation".to_string(),
        );
        runtime.mark_agent_running(info.agent_id, Some("Running...".to_string()));
        let id = info.agent_id;
        *lazy = Some(id);
        Some(id)
    }
}

impl SubAgentEmitter for ClaudeSubAgentEmitter {
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
                    "claude".to_string(),
                    parent_agent_id,
                    display_name,
                );
                info.agent_type = if agent_type.is_empty() { None } else { Some(agent_type) };
                runtime.update_agent_type(info.agent_id, info.agent_type.clone());
                runtime.mark_agent_running(info.agent_id, Some("Running...".to_string()));
                info
            };
            self.agent_runtime_notify.notify_waiters();

            tracing::info!(
                "Claude sub-agent spawned: agent_id={}, conversation_id={}, parent={:?}, tool_use_id={}",
                agent_info.agent_id,
                conversation_id,
                parent_agent_id,
                tool_use_id,
            );

            // Forward sub-agent events to the frontend
            let app = self.app.clone();
            let runtime = Arc::clone(&self.agent_runtime);
            let notify = Arc::clone(&self.agent_runtime_notify);
            let registration = serde_json::json!({
                "kind": "ConversationRegistered",
                "data": {
                    "agent_id": agent_info.agent_id,
                    "workspace_roots": self.workspace_roots,
                    "backend_kind": "claude",
                    "name": &agent_info.name,
                    "agent_type": &agent_info.agent_type,
                    "parent_agent_id": parent_agent_id,
                }
            });
            tokio::spawn(forward_events(
                app.clone(),
                conversation_id,
                event_rx,
                runtime,
                notify,
                registration,
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let tool_use_id = tool_use_id.to_string();
        Box::pin(async move {
            let final_response = final_response
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| text.to_string());

            let (conversation_id, maybe_event) = {
                let mut runtime = self.agent_runtime.lock().await;
                let conv_id = runtime.conversation_id_for_agent(agent_id);
                let current_info = runtime.get_agent(agent_id);
                let already_terminal = current_info
                    .as_ref()
                    .map(|info| info.status.is_terminal())
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
                let should_emit_terminal_event = !already_terminal || final_response_differs;

                if !should_emit_terminal_event || conv_id.is_none() {
                    (conv_id, None)
                } else {
                    let summary = final_response.clone().unwrap_or_else(|| {
                        if success {
                            "Completed".to_string()
                        } else {
                            "Failed".to_string()
                        }
                    });
                    let event = serde_json::json!({
                        "kind": if success { "StreamEnd" } else { "Error" },
                        "data": if success {
                            serde_json::json!({
                                "message": {
                                    "timestamp": crate::claude::unix_now_ms(),
                                    "sender": { "Assistant": { "agent": "Claude" } },
                                    "content": summary,
                                    "tool_calls": [],
                                    "images": [],
                                }
                            })
                        } else {
                            serde_json::json!(summary)
                        }
                    });
                    if let Some(cid) = conv_id {
                        runtime.record_chat_event(cid, &event);
                    }
                    (conv_id, Some(event))
                }
            };
            self.agent_runtime_notify.notify_waiters();

            // Emit to the frontend so the EventRouter updates typing/status.
            if let (Some(conv_id), Some(event)) = (conversation_id, maybe_event) {
                let payload = ChatEventPayload {
                    conversation_id: conv_id,
                    event,
                };
                if let Err(e) = self.app.emit("chat-event", &payload) {
                    tracing::warn!("Failed to emit sub-agent completion event: {e:?}");
                }
            }
            if let Some(conv_id) = conversation_id {
                let typing_payload = ChatEventPayload {
                    conversation_id: conv_id,
                    event: serde_json::json!({
                        "kind": "TypingStatusChanged",
                        "data": false,
                    }),
                };
                if let Err(e) = self.app.emit("chat-event", &typing_payload) {
                    tracing::warn!("Failed to emit sub-agent typing stop event: {e:?}");
                }
            }

            tracing::info!(
                "Claude sub-agent completed: agent_id={}, tool_use_id={}, success={}",
                agent_id,
                tool_use_id,
                success,
            );
        })
    }
}

pub(crate) struct AppState {
    manager: Mutex<ConversationManager>,
    admin: Mutex<AdminManager>,
    terminals: Mutex<TerminalManager>,
    file_watch: SyncMutex<Option<FileWatchManager>>,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    mcp_http_enabled: SyncMutex<bool>,
    debug_mcp_http_enabled: SyncMutex<bool>,
    debug_mcp_http_autoload: SyncMutex<bool>,
    debug_event_log: SyncMutex<DebugEventLog>,
    debug_ui_pending:
        SyncMutex<HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>>>,
    debug_ui_request_seq: AtomicU64,
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
    #[serde(default = "default_debug_mcp_http_enabled")]
    debug_mcp_http_enabled: bool,
    #[serde(default = "default_debug_mcp_http_autoload")]
    debug_mcp_http_autoload: bool,
}

fn default_mcp_http_enabled() -> bool {
    true
}

fn default_debug_mcp_http_enabled() -> bool {
    false
}

fn default_debug_mcp_http_autoload() -> bool {
    false
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            mcp_http_enabled: default_mcp_http_enabled(),
            debug_mcp_http_enabled: default_debug_mcp_http_enabled(),
            debug_mcp_http_autoload: default_debug_mcp_http_autoload(),
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
struct DebugMcpHttpServerSettings {
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
    debug_mcp_http: DebugMcpHttpServerSettings,
}

#[derive(Debug, Clone, Serialize)]
struct DebugUiRequestPayload {
    request_id: String,
    action: String,
    params: Value,
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
    pub(crate) name: Option<String>,
    pub(crate) ephemeral: Option<bool>,
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
    pub(crate) until: Option<String>,
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgentEventsSinceRequest {
    pub(crate) since_seq: Option<u64>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AwaitAgentsRequest {
    pub(crate) agent_ids: Option<Vec<u64>>,
    pub(crate) timeout_ms: Option<u64>,
}

/// Simplified agent result returned by the push-oriented MCP tools.
#[derive(Serialize, Clone)]
pub(crate) struct AgentResult {
    pub(crate) agent_id: u64,
    pub(crate) status: AgentStatus,
    pub(crate) message: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) summary: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct AwaitAgentsResponse {
    pub(crate) ready: Vec<AgentResult>,
    pub(crate) still_running: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitUntil {
    /// Agent is not actively processing: any status except Queued/Running.
    Idle,
    Completed,
    Failed,
    NeedsInput,
    Terminal,
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
            if sibling.exists() {
                tracing::info!("Found subprocess as sibling of current executable");
                return Ok(sibling.to_string_lossy().to_string());
            }
        }
    }
    tracing::warn!("Subprocess not found as sibling of current executable");

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
                if debug.exists() {
                    tracing::info!("Found subprocess in workspace target/debug");
                    return Ok(debug.to_string_lossy().to_string());
                }
                let release = dir.join("target/release/tycode-subprocess");
                if release.exists() {
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

fn load_app_settings() -> AppSettings {
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

fn current_mcp_http_server_settings(enabled: bool) -> McpHttpServerSettings {
    McpHttpServerSettings {
        enabled,
        running: agent_mcp_http::is_agent_mcp_http_server_running(),
        url: agent_mcp_http::agent_mcp_http_server_url(),
    }
}

fn current_debug_mcp_http_server_settings(
    enabled: bool,
    autoload: bool,
) -> DebugMcpHttpServerSettings {
    DebugMcpHttpServerSettings {
        enabled,
        autoload,
        running: debug_mcp_http::is_debug_mcp_http_server_running(),
        url: debug_mcp_http::debug_mcp_http_server_url(),
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
) -> Result<Vec<StartupMcpServer>, String> {
    startup_mcp_servers_for_agent(state, include_agent_control, None)
}

fn startup_mcp_servers_for_agent(
    state: &AppState,
    include_agent_control: bool,
    caller_agent_id: Option<u64>,
) -> Result<Vec<StartupMcpServer>, String> {
    let mut servers = Vec::new();

    if include_agent_control {
        let control_enabled = *state.mcp_http_enabled.lock();
        if !control_enabled {
            return Err("Tyde MCP control server must be enabled for Bridge chats".to_string());
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

    let enabled = *state.debug_mcp_http_enabled.lock();
    let autoload = *state.debug_mcp_http_autoload.lock();
    if !(enabled && autoload) {
        return Ok(servers);
    }

    let Some(url) = debug_mcp_http::debug_mcp_http_server_url() else {
        return Ok(servers);
    };
    if url.trim().is_empty() {
        return Ok(servers);
    }

    servers.push(StartupMcpServer {
        name: "tyde_debug".to_string(),
        transport: StartupMcpTransport::Http {
            url,
            headers: HashMap::new(),
            bearer_token_env_var: None,
        },
    });

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
    let debug_enabled = *state.debug_mcp_http_enabled.lock();
    let debug_autoload = *state.debug_mcp_http_autoload.lock();

    Ok(DebugSnapshot {
        timestamp_ms: now_ms(),
        conversations,
        admin_subprocess_ids,
        terminal_ids,
        runtime_agents,
        agent_mcp_http: current_mcp_http_server_settings(agent_enabled),
        debug_mcp_http: current_debug_mcp_http_server_settings(debug_enabled, debug_autoload),
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

fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn configured_backend_kind() -> Result<BackendKind, String> {
    let backend_raw = std::env::var("TYDE_BACKEND_KIND").or_else(|_| std::env::var("TYDE_BACKEND"));
    match backend_raw {
        Ok(raw) => raw.parse::<BackendKind>(),
        Err(_) => Ok(BackendKind::Tycode),
    }
}

fn resolve_requested_backend_kind(backend_kind: Option<String>) -> Result<BackendKind, String> {
    match backend_kind {
        Some(raw) if !raw.trim().is_empty() => raw.parse::<BackendKind>(),
        _ => configured_backend_kind(),
    }
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
            let remote_roots = parse_remote_workspace_roots(workspace_roots)?;
            match &remote_roots {
                Some((host, _)) => {
                    validate_remote_cli(app, host, "kiro-cli").await?;
                    Ok(host.clone())
                }
                None => Ok(String::new()),
            }
        }
    }
}

fn parse_wait_until(until: Option<String>) -> Result<WaitUntil, String> {
    let Some(raw) = until else {
        return Ok(WaitUntil::Idle);
    };
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "idle" => Ok(WaitUntil::Idle),
        "terminal" | "done" => Ok(WaitUntil::Terminal),
        "completed" | "complete" => Ok(WaitUntil::Completed),
        "failed" | "error" => Ok(WaitUntil::Failed),
        "needs_input" | "needs-input" | "waiting_input" | "waiting-input" | "input" => {
            Ok(WaitUntil::NeedsInput)
        }
        other => Err(format!(
            "Unsupported wait condition '{other}'. Supported values: idle, terminal, completed, failed, needs_input"
        )),
    }
}

fn wait_condition_met(status: AgentStatus, until: WaitUntil) -> bool {
    match until {
        // Typing=false / active-stream state is not currently tracked on AgentInfo.
        // Use status as the best idle proxy:
        // - active: queued/running
        // - idle: waiting_input/completed/failed/cancelled
        WaitUntil::Idle => !matches!(status, AgentStatus::Queued | AgentStatus::Running),
        WaitUntil::Completed => status == AgentStatus::Completed,
        WaitUntil::Failed => status == AgentStatus::Failed,
        WaitUntil::NeedsInput => status == AgentStatus::WaitingInput,
        WaitUntil::Terminal => status.is_terminal(),
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
) -> Result<u64, String> {
    let backend_kind = resolve_requested_backend_kind(backend_kind)?;
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
    let startup_mcp_servers =
        startup_mcp_servers_for_agent(state.inner(), include_agent_control, reserved_agent_id)?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
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
        runtime.mark_agent_running(info.agent_id, Some("Running...".to_string()));
        drop(runtime);
        state.agent_runtime_notify.notify_waiters();
    }

    {
        let mgr = state.manager.lock().await;
        let session = mgr.get(id).ok_or("Conversation not found")?;
        session
            .set_subagent_emitter(Arc::new(ClaudeSubAgentEmitter {
                app: app.clone(),
                agent_runtime: state.agent_runtime.clone(),
                agent_runtime_notify: state.agent_runtime_notify.clone(),
                parent_agent_id: reserved_agent_id,
                lazy_parent_agent_id: Mutex::new(None),
                parent_conversation_id: id,
                workspace_roots: workspace_roots.clone(),
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

    tokio::spawn(forward_events(
        app.clone(),
        id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
    ));
    Ok(id)
}

async fn forward_events(
    app: tauri::AppHandle,
    conversation_id: u64,
    mut rx: mpsc::UnboundedReceiver<Value>,
    agent_runtime: Arc<Mutex<AgentRuntime>>,
    agent_runtime_notify: Arc<Notify>,
    registration: Value,
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
        let changed = {
            let mut runtime = agent_runtime.lock().await;
            runtime.record_chat_event(conversation_id, &event)
        };
        if changed {
            agent_runtime_notify.notify_waiters();
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
    state: tauri::State<'_, AppState>,
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
    }
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

    let backend_kind = resolve_requested_backend_kind(backend_kind)?;
    let ephemeral = ephemeral.unwrap_or(false);
    let resolved_path =
        resolve_backend_executable_path(app, &workspace_roots, backend_kind).await?;
    let startup_mcp_servers = startup_mcp_servers_for_new_sessions(state, false)?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        ephemeral,
        &startup_mcp_servers,
    )
    .await?;

    let display_name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

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
            display_name.unwrap_or_else(|| format!("Agent {conversation_id}")),
        )
    };
    state.agent_runtime_notify.notify_waiters();

    // Set up the sub-agent emitter AFTER registration so we know this agent's id.
    // Sub-agents spawned by this agent will have parent_agent_id = info.agent_id.
    {
        let mgr = state.manager.lock().await;
        let session = mgr.get(conversation_id).ok_or("Conversation not found")?;
        session
            .set_subagent_emitter(Arc::new(ClaudeSubAgentEmitter {
                app: app.clone(),
                agent_runtime: state.agent_runtime.clone(),
                agent_runtime_notify: state.agent_runtime_notify.clone(),
                parent_agent_id: Some(info.agent_id),
                lazy_parent_agent_id: Mutex::new(None),
                parent_conversation_id: conversation_id,
                workspace_roots: workspace_roots.clone(),
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

    tokio::spawn(forward_events(
        app.clone(),
        conversation_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
    ));

    execute_conversation_command(
        app,
        state,
        conversation_id,
        SessionCommand::SendMessage {
            message: prompt,
            images: None,
        },
    )
    .await?;

    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_agent_running(info.agent_id, Some("Running...".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
    }

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

    let changed = {
        let mut runtime = state.agent_runtime.lock().await;
        runtime.mark_agent_running(agent_id, Some("Running...".to_string()))
    };
    if changed {
        state.agent_runtime_notify.notify_waiters();
    }

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
            .filter(|c| !c.status.is_terminal())
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
    }

    Ok(())
}

pub(crate) async fn terminate_agent_internal(
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
            .filter(|c| !c.status.is_terminal())
            .map(|c| c.agent_id)
            .collect()
    };
    for child_id in child_ids {
        let _ = Box::pin(terminate_agent_internal(
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
    }

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
    let WaitForAgentRequest {
        agent_id,
        until,
        timeout_ms,
    } = request;
    let wait_until = parse_wait_until(until)?;
    let idle_timeout = timeout_ms.unwrap_or(60_000).clamp(1, 30 * 60 * 1000);
    let idle_duration = tokio::time::Duration::from_millis(idle_timeout);
    // Cap total wall time at 10x the idle timeout to prevent infinite waits.
    let max_wall = idle_duration.saturating_mul(10);
    let wall_deadline = tokio::time::Instant::now() + max_wall;
    let mut idle_deadline = tokio::time::Instant::now() + idle_duration;
    let mut last_updated_at_ms: Option<u64> = None;

    loop {
        let current = {
            let runtime = state.agent_runtime.lock().await;
            runtime
                .get_agent(agent_id)
                .ok_or(format!("Agent {agent_id} not found"))?
        };
        if wait_condition_met(current.status.clone(), wait_until) {
            return Ok(current);
        }

        // Extend idle deadline when agent shows new activity.
        if last_updated_at_ms.is_none_or(|prev| current.updated_at_ms > prev) {
            idle_deadline = tokio::time::Instant::now() + idle_duration;
            last_updated_at_ms = Some(current.updated_at_ms);
        }

        let notified = state.agent_runtime_notify.notified();
        let now = tokio::time::Instant::now();
        let effective_deadline = idle_deadline.min(wall_deadline);
        if now >= effective_deadline {
            return Err(format!("Timed out waiting for agent {agent_id}"));
        }
        let remaining = effective_deadline.saturating_duration_since(now);
        match tokio::time::timeout(remaining, notified).await {
            Ok(_) => {}
            Err(_) => return Err(format!("Timed out waiting for agent {agent_id}")),
        }
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
        status: info.status.clone(),
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
        until: Some("idle".to_string()),
        timeout_ms: None,
    };
    let info = wait_for_agent_internal(state, wait_request).await?;
    Ok(agent_result_from_info(&info))
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
        let (ready, still_running, newest_updated_at) = {
            let runtime = state.agent_runtime.lock().await;
            let watch_ids: Vec<u64> = match &request.agent_ids {
                Some(ids) => {
                    // Validate all requested IDs exist.
                    for &id in ids {
                        if !runtime.has_agent(id) {
                            return Err(format!("Agent {id} not found"));
                        }
                    }
                    ids.clone()
                }
                None => {
                    // Watch all non-terminal agents.
                    runtime
                        .list_agents()
                        .iter()
                        .filter(|a| !a.status.is_terminal())
                        .map(|a| a.agent_id)
                        .collect()
                }
            };

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
                let is_idle = !matches!(info.status, AgentStatus::Queued | AgentStatus::Running);
                if is_idle {
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

        let notified = state.agent_runtime_notify.notified();
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
    }

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
    name: Option<String>,
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
async fn terminate_agent(state: tauri::State<'_, AppState>, agent_id: u64) -> Result<(), String> {
    terminate_agent_internal(state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn get_agent(
    state: tauri::State<'_, AppState>,
    agent_id: u64,
) -> Result<Option<AgentInfo>, String> {
    get_agent_internal(state.inner(), AgentIdRequest { agent_id }).await
}

#[tauri::command]
async fn list_agents(state: tauri::State<'_, AppState>) -> Result<Vec<AgentInfo>, String> {
    list_agents_internal(state.inner()).await
}

#[tauri::command]
async fn wait_for_agent(
    state: tauri::State<'_, AppState>,
    agent_id: u64,
    until: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<AgentInfo, String> {
    wait_for_agent_internal(
        state.inner(),
        WaitForAgentRequest {
            agent_id,
            until,
            timeout_ms,
        },
    )
    .await
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
    let debug_enabled = *state.debug_mcp_http_enabled.lock();
    let debug_autoload = *state.debug_mcp_http_autoload.lock();
    {
        *state.mcp_http_enabled.lock() = enabled;
    }

    save_app_settings(&AppSettings {
        mcp_http_enabled: enabled,
        debug_mcp_http_enabled: debug_enabled,
        debug_mcp_http_autoload: debug_autoload,
    })?;

    if enabled {
        if let Err(err) = agent_mcp_http::start_agent_mcp_http_server(&app) {
            *state.mcp_http_enabled.lock() = false;
            if let Err(e) = save_app_settings(&AppSettings {
                mcp_http_enabled: false,
                debug_mcp_http_enabled: debug_enabled,
                debug_mcp_http_autoload: debug_autoload,
            }) {
                tracing::error!(
                    "Failed to revert app settings after MCP HTTP server start failure: {e}"
                );
            }
            return Err(format!("Failed to start MCP HTTP server: {err}"));
        }
    } else {
        agent_mcp_http::stop_agent_mcp_http_server();
    }

    let status = current_mcp_http_server_settings(enabled);
    if enabled && !status.running {
        *state.mcp_http_enabled.lock() = false;
        if let Err(e) = save_app_settings(&AppSettings {
            mcp_http_enabled: false,
            debug_mcp_http_enabled: debug_enabled,
            debug_mcp_http_autoload: debug_autoload,
        }) {
            tracing::error!(
                "Failed to revert app settings after MCP HTTP server start failure: {e}"
            );
        }
        return Err("Failed to start MCP HTTP server".to_string());
    }
    Ok(status)
}

#[tauri::command]
fn get_debug_mcp_http_server_settings(
    state: tauri::State<'_, AppState>,
) -> Result<DebugMcpHttpServerSettings, String> {
    let enabled = *state.debug_mcp_http_enabled.lock();
    let autoload = *state.debug_mcp_http_autoload.lock();
    Ok(current_debug_mcp_http_server_settings(enabled, autoload))
}

#[tauri::command]
fn set_debug_mcp_http_server_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<DebugMcpHttpServerSettings, String> {
    let agent_enabled = *state.mcp_http_enabled.lock();
    let mut debug_autoload = *state.debug_mcp_http_autoload.lock();
    if !enabled {
        debug_autoload = false;
        *state.debug_mcp_http_autoload.lock() = false;
    }
    {
        *state.debug_mcp_http_enabled.lock() = enabled;
    }

    save_app_settings(&AppSettings {
        mcp_http_enabled: agent_enabled,
        debug_mcp_http_enabled: enabled,
        debug_mcp_http_autoload: debug_autoload,
    })?;

    if enabled {
        if let Err(err) = debug_mcp_http::start_debug_mcp_http_server(&app) {
            *state.debug_mcp_http_enabled.lock() = false;
            if let Err(e) = save_app_settings(&AppSettings {
                mcp_http_enabled: agent_enabled,
                debug_mcp_http_enabled: false,
                debug_mcp_http_autoload: false,
            }) {
                tracing::error!(
                    "Failed to revert app settings after debug MCP HTTP server start failure: {e}"
                );
            }
            return Err(format!("Failed to start debug MCP HTTP server: {err}"));
        }
    } else {
        debug_mcp_http::stop_debug_mcp_http_server();
    }

    let status = current_debug_mcp_http_server_settings(enabled, debug_autoload);
    if enabled && !status.running {
        *state.debug_mcp_http_enabled.lock() = false;
        *state.debug_mcp_http_autoload.lock() = false;
        if let Err(e) = save_app_settings(&AppSettings {
            mcp_http_enabled: agent_enabled,
            debug_mcp_http_enabled: false,
            debug_mcp_http_autoload: false,
        }) {
            tracing::error!(
                "Failed to revert app settings after debug MCP HTTP server start failure: {e}"
            );
        }
        return Err("Failed to start debug MCP HTTP server".to_string());
    }
    Ok(status)
}

#[tauri::command]
fn set_debug_mcp_http_server_autoload_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<DebugMcpHttpServerSettings, String> {
    let agent_enabled = *state.mcp_http_enabled.lock();
    let debug_enabled = *state.debug_mcp_http_enabled.lock();
    if enabled && !debug_enabled {
        return Err("Enable debug MCP server before enabling auto-load".to_string());
    }

    let autoload = enabled && debug_enabled;
    {
        *state.debug_mcp_http_autoload.lock() = autoload;
    }

    save_app_settings(&AppSettings {
        mcp_http_enabled: agent_enabled,
        debug_mcp_http_enabled: debug_enabled,
        debug_mcp_http_autoload: autoload,
    })?;

    Ok(current_debug_mcp_http_server_settings(
        debug_enabled,
        autoload,
    ))
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
        SessionCommand::ResumeSession { session_id },
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
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::DeleteSession { session_id },
    )
    .await
}

#[tauri::command]
async fn get_session_id(
    state: tauri::State<'_, AppState>,
    conversation_id: u64,
) -> Result<Option<String>, String> {
    let mgr = state.manager.lock().await;
    let session = mgr.get(conversation_id).ok_or("Conversation not found")?;
    Ok(session.session_id().await)
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
) -> Result<(), String> {
    execute_conversation_command(
        &app,
        &state,
        conversation_id,
        SessionCommand::UpdateSettings {
            settings,
            persist: true,
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
    let backend_kind = resolve_requested_backend_kind(backend_kind)?;
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
    execute_admin_command(
        &state,
        admin_id,
        SessionCommand::DeleteSession { session_id },
    )
    .await
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
async fn git_worktree_add(
    working_dir: String,
    path: String,
    branch: String,
) -> Result<(), String> {
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
    let startup_mcp_servers = startup_mcp_servers_for_new_sessions(state.inner(), false)?;
    let (session, rx) = BackendSession::spawn(
        backend_kind,
        &resolved_path,
        &workspace_roots,
        false,
        &startup_mcp_servers,
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

    tokio::spawn(forward_events(
        app,
        conversation_id,
        rx,
        state.agent_runtime.clone(),
        state.agent_runtime_notify.clone(),
        registration,
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

    {
        let mut mgr = state.terminals.lock().await;
        mgr.close_all();
    }
    Ok(())
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
        .args(["-l", "-c", "echo $PATH"])
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

pub fn run() {
    resolve_shell_path();
    let mut app_settings = load_app_settings();
    if !app_settings.debug_mcp_http_enabled {
        app_settings.debug_mcp_http_autoload = false;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            manager: Mutex::new(ConversationManager::new()),
            admin: Mutex::new(AdminManager::new()),
            terminals: Mutex::new(TerminalManager::new()),
            file_watch: SyncMutex::new(None),
            agent_runtime: Arc::new(Mutex::new(AgentRuntime::new())),
            agent_runtime_notify: Arc::new(Notify::new()),
            mcp_http_enabled: SyncMutex::new(app_settings.mcp_http_enabled),
            debug_mcp_http_enabled: SyncMutex::new(app_settings.debug_mcp_http_enabled),
            debug_mcp_http_autoload: SyncMutex::new(app_settings.debug_mcp_http_autoload),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
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
            let debug_mcp_http_enabled = *app.state::<AppState>().debug_mcp_http_enabled.lock();
            if debug_mcp_http_enabled {
                if let Err(err) = debug_mcp_http::start_debug_mcp_http_server(app.handle()) {
                    tracing::warn!("Debug MCP HTTP server failed to start: {err}");
                }
            } else {
                tracing::info!("Debug MCP HTTP server disabled by app settings");
            }
            if let Some(window) = app.get_webview_window("main") {
                let window_handle = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_handle.hide();
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            create_conversation,
            send_message,
            cancel_conversation,
            close_conversation,
            spawn_agent,
            send_agent_message,
            interrupt_agent,
            terminate_agent,
            get_agent,
            list_agents,
            wait_for_agent,
            agent_events_since,
            collect_agent_result,
            get_mcp_http_server_settings,
            set_mcp_http_server_enabled,
            get_debug_mcp_http_server_settings,
            set_debug_mcp_http_server_enabled,
            set_debug_mcp_http_server_autoload_enabled,
            submit_debug_ui_response,
            get_settings,
            list_models,
            list_sessions,
            resume_session,
            delete_session,
            get_session_id,
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
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use serde_json::json;

    fn test_app_state() -> AppState {
        AppState {
            manager: Mutex::new(ConversationManager::new()),
            admin: Mutex::new(AdminManager::new()),
            terminals: Mutex::new(TerminalManager::new()),
            file_watch: SyncMutex::new(None),
            agent_runtime: Arc::new(Mutex::new(AgentRuntime::new())),
            agent_runtime_notify: Arc::new(Notify::new()),
            mcp_http_enabled: SyncMutex::new(true),
            debug_mcp_http_enabled: SyncMutex::new(false),
            debug_mcp_http_autoload: SyncMutex::new(false),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
            debug_ui_pending: SyncMutex::new(HashMap::new()),
            debug_ui_request_seq: AtomicU64::new(1),
        }
    }

    #[test]
    fn parse_wait_until_defaults_to_idle() {
        assert_eq!(parse_wait_until(None).unwrap(), WaitUntil::Idle);
        assert_eq!(parse_wait_until(Some("".into())).unwrap(), WaitUntil::Idle);
        assert_eq!(
            parse_wait_until(Some("idle".into())).unwrap(),
            WaitUntil::Idle
        );
    }

    #[test]
    fn parse_wait_until_explicit_values_preserved() {
        assert_eq!(
            parse_wait_until(Some("terminal".into())).unwrap(),
            WaitUntil::Terminal
        );
        assert_eq!(
            parse_wait_until(Some("done".into())).unwrap(),
            WaitUntil::Terminal
        );
        assert_eq!(
            parse_wait_until(Some("completed".into())).unwrap(),
            WaitUntil::Completed
        );
        assert_eq!(
            parse_wait_until(Some("complete".into())).unwrap(),
            WaitUntil::Completed
        );
        assert_eq!(
            parse_wait_until(Some("failed".into())).unwrap(),
            WaitUntil::Failed
        );
        assert_eq!(
            parse_wait_until(Some("error".into())).unwrap(),
            WaitUntil::Failed
        );
        assert_eq!(
            parse_wait_until(Some("needs_input".into())).unwrap(),
            WaitUntil::NeedsInput
        );
        assert_eq!(
            parse_wait_until(Some("waiting-input".into())).unwrap(),
            WaitUntil::NeedsInput
        );
        assert_eq!(
            parse_wait_until(Some("input".into())).unwrap(),
            WaitUntil::NeedsInput
        );
    }

    #[test]
    fn parse_wait_until_rejects_unknown() {
        let err = parse_wait_until(Some("bogus".into())).unwrap_err();
        assert!(err.contains("Unsupported wait condition 'bogus'"));
        assert!(err.contains("Supported values: idle, terminal, completed, failed, needs_input"));
    }

    #[test]
    fn wait_condition_idle_matches_non_active_statuses() {
        // Idle = not Queued and not Running
        assert!(!wait_condition_met(AgentStatus::Queued, WaitUntil::Idle));
        assert!(!wait_condition_met(AgentStatus::Running, WaitUntil::Idle));
        assert!(wait_condition_met(
            AgentStatus::WaitingInput,
            WaitUntil::Idle
        ));
        assert!(wait_condition_met(AgentStatus::Completed, WaitUntil::Idle));
        assert!(wait_condition_met(AgentStatus::Failed, WaitUntil::Idle));
        assert!(wait_condition_met(AgentStatus::Cancelled, WaitUntil::Idle));
    }

    #[test]
    fn wait_condition_terminal_unchanged() {
        assert!(!wait_condition_met(
            AgentStatus::Queued,
            WaitUntil::Terminal
        ));
        assert!(!wait_condition_met(
            AgentStatus::Running,
            WaitUntil::Terminal
        ));
        assert!(!wait_condition_met(
            AgentStatus::WaitingInput,
            WaitUntil::Terminal
        ));
        assert!(wait_condition_met(
            AgentStatus::Completed,
            WaitUntil::Terminal
        ));
        assert!(wait_condition_met(AgentStatus::Failed, WaitUntil::Terminal));
        assert!(wait_condition_met(
            AgentStatus::Cancelled,
            WaitUntil::Terminal
        ));
    }

    #[test]
    fn wait_condition_specific_statuses() {
        assert!(wait_condition_met(
            AgentStatus::Completed,
            WaitUntil::Completed
        ));
        assert!(!wait_condition_met(
            AgentStatus::Failed,
            WaitUntil::Completed
        ));
        assert!(wait_condition_met(AgentStatus::Failed, WaitUntil::Failed));
        assert!(!wait_condition_met(
            AgentStatus::Completed,
            WaitUntil::Failed
        ));
        assert!(wait_condition_met(
            AgentStatus::WaitingInput,
            WaitUntil::NeedsInput
        ));
        assert!(!wait_condition_met(
            AgentStatus::Running,
            WaitUntil::NeedsInput
        ));
    }

    #[tokio::test]
    async fn wait_for_agent_without_until_waits_for_idle() {
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
            assert!(runtime.mark_agent_running(info.agent_id, Some("Running...".into())));
            info.agent_id
        };

        let wait_fut = wait_for_agent_internal(
            &state,
            WaitForAgentRequest {
                agent_id,
                until: None,
                timeout_ms: Some(1_000),
            },
        );
        let notifier = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            {
                let mut runtime = state.agent_runtime.lock().await;
                let changed = runtime.record_chat_event(
                    conversation_id,
                    &json!({
                        "kind": "ToolRequest",
                        "data": { "tool_name": "ask_user_question" }
                    }),
                );
                assert!(changed);
            }
            state.agent_runtime_notify.notify_waiters();
        };
        let (result, _) = tokio::join!(wait_fut, notifier);
        let agent = result.expect("wait_for_agent should return once agent is idle");
        assert_eq!(agent.status, AgentStatus::WaitingInput);
    }

    #[tokio::test]
    async fn wait_for_agent_explicit_terminal_still_works() {
        let state = test_app_state();
        let conversation_id = 9002;
        let agent_id = {
            let mut runtime = state.agent_runtime.lock().await;
            let info = runtime.register_agent(
                conversation_id,
                vec!["/tmp".into()],
                "tycode".into(),
                None,
                "test".into(),
            );
            assert!(runtime.mark_agent_running(info.agent_id, Some("Running...".into())));
            info.agent_id
        };

        let wait_fut = wait_for_agent_internal(
            &state,
            WaitForAgentRequest {
                agent_id,
                until: Some("terminal".to_string()),
                timeout_ms: Some(1_000),
            },
        );
        let notifier = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            {
                let mut runtime = state.agent_runtime.lock().await;
                let changed = runtime.record_chat_event(
                    conversation_id,
                    &json!({
                        "kind": "StreamEnd",
                        "data": { "message": { "content": "Done." } }
                    }),
                );
                assert!(changed);
            }
            state.agent_runtime_notify.notify_waiters();
        };
        let (result, _) = tokio::join!(wait_fut, notifier);
        let agent = result.expect("terminal wait should still resolve");
        assert_eq!(agent.status, AgentStatus::Completed);
    }

    #[tokio::test]
    async fn wait_for_agent_invalid_until_returns_clear_error() {
        let state = test_app_state();
        let err = wait_for_agent_internal(
            &state,
            WaitForAgentRequest {
                agent_id: 42,
                until: Some("bogus".into()),
                timeout_ms: Some(1_000),
            },
        )
        .await
        .expect_err("invalid wait condition should fail fast");
        assert!(err.contains("Unsupported wait condition 'bogus'"));
        assert!(err.contains("Supported values: idle, terminal, completed, failed, needs_input"));
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
