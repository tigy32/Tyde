use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::protocol::{
    ClientFrame, ConversationSnapshot, HandshakeResult, ServerFrame, PROTOCOL_VERSION, TYDE_VERSION,
};
use crate::remote::{open_ssh_unix_socket_tunnel, parse_remote_path, to_remote_uri};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    Disconnected { reason: String },
}

#[derive(Debug, Clone, Serialize)]
struct ConnectionStateEvent {
    host_id: String,
    state: ConnectionState,
}

#[derive(Debug, Clone, Serialize)]
struct VersionWarningEvent {
    host_id: String,
    host: String,
    local_version: String,
    remote_version: String,
}

struct PendingRequest {
    tx: oneshot::Sender<Result<Value, String>>,
}

pub struct TydeServerConnection {
    pub host_id: String,
    ssh_host: String,
    remote_socket_path: String,
    app: tauri::AppHandle,
    stdin_tx: Mutex<Option<tokio::sync::mpsc::Sender<String>>>,
    pending_requests: Mutex<HashMap<u64, PendingRequest>>,
    next_req_id: AtomicU64,
    state: Mutex<ConnectionState>,
    last_agent_event_seq: AtomicU64,
    last_chat_event_seqs: Mutex<HashMap<u64, u64>>,
    remote_instance_id: Mutex<Option<String>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    writer_handle: Mutex<Option<JoinHandle<()>>>,
    tunnel_child: Mutex<Option<tokio::process::Child>>,
    tunnel_socket_path: Mutex<Option<PathBuf>>,
    pub remote_conversations: Mutex<Vec<ConversationSnapshot>>,
    /// Server-authoritative session records, synced from handshake.
    pub remote_session_records: Mutex<Vec<crate::session_store::SessionRecord>>,
    /// Server-authoritative project records, synced from handshake.
    pub remote_projects: Mutex<Vec<crate::project_store::ProjectRecord>>,
    /// Maps server-side conversation IDs to local conversation IDs.
    server_to_local_conv: Mutex<HashMap<u64, u64>>,
    /// Agent IDs known to be managed by this remote server.
    remote_agent_ids: Mutex<std::collections::HashSet<String>>,
    /// Server-side conversation mapping for each known remote agent.
    agent_to_server_conversation: Mutex<HashMap<String, u64>>,
}

impl TydeServerConnection {
    pub async fn connect(
        app: tauri::AppHandle,
        host_id: String,
        ssh_host: String,
        remote_socket_path: String,
    ) -> Result<Arc<Self>, String> {
        let conn = Arc::new(Self {
            host_id,
            ssh_host,
            remote_socket_path,
            app,
            stdin_tx: Mutex::new(None),
            pending_requests: Mutex::new(HashMap::new()),
            next_req_id: AtomicU64::new(1),
            state: Mutex::new(ConnectionState::Connecting),
            last_agent_event_seq: AtomicU64::new(0),
            last_chat_event_seqs: Mutex::new(HashMap::new()),
            remote_instance_id: Mutex::new(None),
            reader_handle: Mutex::new(None),
            writer_handle: Mutex::new(None),
            tunnel_child: Mutex::new(None),
            tunnel_socket_path: Mutex::new(None),
            remote_conversations: Mutex::new(Vec::new()),
            remote_session_records: Mutex::new(Vec::new()),
            remote_projects: Mutex::new(Vec::new()),
            server_to_local_conv: Mutex::new(HashMap::new()),
            remote_agent_ids: Mutex::new(std::collections::HashSet::new()),
            agent_to_server_conversation: Mutex::new(HashMap::new()),
        });

        conn.establish_connection().await?;
        Ok(conn)
    }

    pub async fn register_conversation_mapping(&self, server_id: u64, local_id: u64) {
        self.server_to_local_conv
            .lock()
            .await
            .insert(server_id, local_id);
    }

    pub async fn register_remote_agent_id(&self, agent_id: String) {
        self.remote_agent_ids.lock().await.insert(agent_id);
    }

    /// Detach local ownership metadata for a remote agent that was cancelled/
    /// terminated. Returns the mapped local conversation ID, if any.
    pub async fn detach_remote_agent(&self, agent_id: &str) -> Option<u64> {
        self.remote_agent_ids.lock().await.remove(agent_id);

        let server_cid = self
            .agent_to_server_conversation
            .lock()
            .await
            .remove(agent_id)?;

        let local_cid = self
            .server_to_local_conv
            .lock()
            .await
            .get(&server_cid)
            .copied();

        let has_other_agents_for_conversation = self
            .agent_to_server_conversation
            .lock()
            .await
            .values()
            .any(|&cid| cid == server_cid);

        if !has_other_agents_for_conversation {
            self.server_to_local_conv.lock().await.remove(&server_cid);
        }

        local_cid
    }

    pub fn ssh_host(&self) -> &str {
        &self.ssh_host
    }

    fn normalize_workspace_root(&self, root: &str) -> String {
        if parse_remote_path(root).is_some() {
            root.to_string()
        } else if root.starts_with('/') {
            to_remote_uri(self.ssh_host(), root)
        } else {
            root.to_string()
        }
    }

    pub fn normalize_workspace_roots(&self, roots: &[String]) -> Vec<String> {
        roots
            .iter()
            .map(|root| self.normalize_workspace_root(root))
            .collect()
    }

    pub fn normalize_project_record(
        &self,
        mut record: crate::project_store::ProjectRecord,
    ) -> crate::project_store::ProjectRecord {
        record.workspace_path = self.normalize_workspace_root(&record.workspace_path);
        record.roots = self.normalize_workspace_roots(&record.roots);
        record
    }

    fn normalize_workspace_roots_json_array(
        &self,
        roots: &[serde_json::Value],
    ) -> Option<Vec<serde_json::Value>> {
        let mut normalized = Vec::with_capacity(roots.len());
        for root in roots {
            let root_str = root.as_str()?;
            normalized.push(serde_json::Value::String(
                self.normalize_workspace_root(root_str),
            ));
        }
        Some(normalized)
    }

    pub async fn translate_conversation_id(&self, server_id: u64) -> Option<u64> {
        self.server_to_local_conv
            .lock()
            .await
            .get(&server_id)
            .copied()
    }

    pub async fn to_local_conversation_id(&self, server_id: u64) -> Result<u64, String> {
        self.translate_conversation_id(server_id)
            .await
            .ok_or_else(|| {
                format!("No local conversation mapping for server conversation {server_id}")
            })
    }

    async fn establish_connection(self: &Arc<Self>) -> Result<(), String> {
        establish_connection_impl(Arc::clone(self)).await
    }

    async fn handle_server_frame(self: &Arc<Self>, frame: ServerFrame) {
        match frame {
            ServerFrame::Result { req_id, data } => {
                if let Some(pending) = self.pending_requests.lock().await.remove(&req_id) {
                    let _ = pending.tx.send(Ok(data));
                }
            }
            ServerFrame::Error { req_id, error } => {
                if let Some(pending) = self.pending_requests.lock().await.remove(&req_id) {
                    let _ = pending.tx.send(Err(error));
                }
            }
            ServerFrame::Event {
                event,
                seq,
                payload,
            } => {
                // Track remote agent IDs and advance agent event seq cursor
                if event == "agent-changed" {
                    if let Some(aid) = payload
                        .get("agent_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                    {
                        self.remote_agent_ids.lock().await.insert(aid.clone());
                        if let Some(server_cid) =
                            payload.get("conversation_id").and_then(|v| v.as_u64())
                        {
                            self.agent_to_server_conversation
                                .lock()
                                .await
                                .insert(aid, server_cid);
                        }
                    }
                    if let Some(s) = seq {
                        let current = self.last_agent_event_seq.load(Ordering::Relaxed);
                        if s > current {
                            self.last_agent_event_seq.store(s, Ordering::Relaxed);
                        }
                    }
                }

                // Translate server conversation IDs to local IDs before re-emitting.
                let mut server_cid_for_cursor = None;
                let translated = if let Some(server_cid) =
                    payload.get("conversation_id").and_then(|v| v.as_u64())
                {
                    let mut mapped_local = self.translate_conversation_id(server_cid).await;

                    // If this is a brand-new remote agent/conversation created
                    // by another client after our handshake, opportunistically
                    // materialize a local proxy from the agent payload.
                    if mapped_local.is_none() && event == "agent-changed" {
                        if let Some(backend_kind) =
                            payload.get("backend_kind").and_then(|v| v.as_str())
                        {
                            if let Some(arr) =
                                payload.get("workspace_roots").and_then(|v| v.as_array())
                            {
                                let roots: Vec<String> = arr
                                    .iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect();
                                if roots.len() == arr.len() {
                                    let app_state = self.app.state::<crate::AppState>();
                                    if let Err(e) = crate::materialize_remote_conversation(
                                        &self.app,
                                        &app_state,
                                        Arc::clone(self),
                                        server_cid,
                                        backend_kind,
                                        &roots,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "Failed to materialize server conversation {} from agent-changed: {}",
                                            server_cid,
                                            e
                                        );
                                    } else {
                                        mapped_local =
                                            self.translate_conversation_id(server_cid).await;
                                    }
                                }
                            }
                        }
                    }

                    server_cid_for_cursor = Some(server_cid);
                    match mapped_local {
                        Some(local_cid) => {
                            let mut p = payload.clone();
                            if let Some(obj) = p.as_object_mut() {
                                obj.insert(
                                    "conversation_id".to_string(),
                                    serde_json::json!(local_cid),
                                );
                                if let Some(arr) = obj
                                    .get("workspace_roots")
                                    .and_then(|v| v.as_array())
                                    .and_then(|arr| self.normalize_workspace_roots_json_array(arr))
                                {
                                    obj.insert(
                                        "workspace_roots".to_string(),
                                        serde_json::Value::Array(arr),
                                    );
                                }

                                // chat-event payloads can carry nested workspace roots
                                // at payload.event.data.workspace_roots (e.g. ConversationRegistered).
                                if event == "chat-event" {
                                    let normalized_nested = p
                                        .get("event")
                                        .and_then(|e| e.get("data"))
                                        .and_then(|d| d.get("workspace_roots"))
                                        .and_then(|v| v.as_array())
                                        .and_then(|arr| {
                                            self.normalize_workspace_roots_json_array(arr)
                                        });
                                    if let Some(normalized_nested) = normalized_nested {
                                        if let Some(data_obj) = p
                                            .get_mut("event")
                                            .and_then(serde_json::Value::as_object_mut)
                                            .and_then(|event_obj| event_obj.get_mut("data"))
                                            .and_then(serde_json::Value::as_object_mut)
                                        {
                                            data_obj.insert(
                                                "workspace_roots".to_string(),
                                                serde_json::Value::Array(normalized_nested),
                                            );
                                        }
                                    }
                                }
                            }
                            p
                        }
                        None => {
                            tracing::warn!(
                                "Dropping event '{event}': no local mapping for server conversation {server_cid}"
                            );
                            return;
                        }
                    }
                } else {
                    payload
                };

                // Track chat replay cursor only for chat-event frames, and only
                // after translation succeeds so dropped events don't advance the
                // replay position.
                if event == "chat-event" {
                    if let (Some(s), Some(server_cid)) = (seq, server_cid_for_cursor) {
                        self.last_chat_event_seqs.lock().await.insert(server_cid, s);
                    }

                    // Route chat events through the unified forward_events
                    // pipeline so session store tracking (message counts,
                    // aliases, etc.) works identically for remote sessions.
                    if let Some(local_cid) =
                        translated.get("conversation_id").and_then(|v| v.as_u64())
                    {
                        if let Some(inner_event) = translated.get("event").cloned() {
                            let app_state = self.app.state::<crate::AppState>();
                            let sender = app_state
                                .remote_chat_senders
                                .lock()
                                .get(&local_cid)
                                .cloned();
                            if let Some(tx) = sender {
                                let _ = tx.send(inner_event);
                            } else {
                                tracing::error!(
                                    "No remote_chat_sender for local conversation {local_cid} — \
                                     session store tracking will be missing"
                                );
                                let _ = self.app.emit(&event, &translated);
                            }
                        }
                    }
                } else if event == "tyde-projects-changed" {
                    let projects: Vec<crate::project_store::ProjectRecord> =
                        serde_json::from_value(
                            translated.get("projects").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                    let normalized: Vec<_> = projects
                        .into_iter()
                        .map(|r| self.normalize_project_record(r))
                        .collect();
                    let _ = self.app.emit(
                        "tyde-projects-changed",
                        serde_json::json!({
                            "host": self.ssh_host(),
                            "projects": normalized
                        }),
                    );
                } else {
                    let _ = self.app.emit(&event, &translated);
                }
            }
            ServerFrame::Shutdown { reason } => {
                tracing::info!("Remote Tyde server shutting down: {reason}");
                self.emit_state(ConnectionState::Disconnected { reason })
                    .await;
            }
        }
    }

    /// Send a command to the remote Tyde server and wait for the response.
    pub async fn invoke(&self, command: &str, params: Value) -> Result<Value, String> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);

        let frame = ClientFrame::Invoke {
            req_id,
            command: command.to_string(),
            params,
        };

        let json =
            serde_json::to_string(&frame).map_err(|e| format!("Failed to serialize: {e}"))?;

        // Clone the sender so we don't hold the stdin_tx lock across the
        // async send — that would block reconnection under backpressure.
        let sender = {
            let guard = self.stdin_tx.lock().await;
            guard.as_ref().ok_or("Not connected")?.clone()
        };

        let (tx, rx) = oneshot::channel();
        self.pending_requests
            .lock()
            .await
            .insert(req_id, PendingRequest { tx });

        if let Err(e) = sender.send(json).await {
            // Clean up the pending request on send failure
            self.pending_requests.lock().await.remove(&req_id);
            return Err(format!("Connection lost: {e}"));
        }

        rx.await.map_err(|_| "Connection lost".to_string())?
    }

    pub async fn connection_state(&self) -> ConnectionState {
        self.state.lock().await.clone()
    }

    pub async fn owns_agent(&self, agent_id: &str) -> bool {
        self.remote_agent_ids.lock().await.contains(agent_id)
    }

    pub async fn fetch_session_records(
        &self,
    ) -> Result<Vec<crate::session_store::SessionRecord>, String> {
        let resp = self
            .invoke("list_session_records", serde_json::json!({}))
            .await?;
        let records: Vec<crate::session_store::SessionRecord> = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse remote session records: {e}"))?;
        *self.remote_session_records.lock().await = records.clone();
        Ok(records)
    }

    pub async fn owns_session_record(&self, id: &str) -> bool {
        self.remote_session_records
            .lock()
            .await
            .iter()
            .any(|r| r.id == id)
    }

    #[allow(dead_code)]
    pub async fn fetch_projects(&self) -> Result<Vec<crate::project_store::ProjectRecord>, String> {
        let resp = self.invoke("list_projects", serde_json::json!({})).await?;
        let records: Vec<crate::project_store::ProjectRecord> = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse remote projects: {e}"))?;
        *self.remote_projects.lock().await = records.clone();
        Ok(records)
    }

    pub async fn fetch_remote_agents(
        self: &Arc<Self>,
    ) -> Result<Vec<crate::agent_runtime::AgentInfo>, String> {
        let resp = self.invoke("list_agents", serde_json::json!({})).await?;
        let agents: Vec<crate::agent_runtime::AgentInfo> = serde_json::from_value(resp)
            .map_err(|e| format!("Failed to parse remote agent list: {e}"))?;

        let mut out = Vec::with_capacity(agents.len());
        let mut fresh_agent_ids = HashSet::new();
        let mut fresh_agent_to_server_cid = HashMap::new();
        for mut agent in agents {
            let server_cid = agent.conversation_id;
            let local_cid = if let Some(local_cid) =
                self.translate_conversation_id(server_cid).await
            {
                local_cid
            } else {
                let app_state = self.app.state::<crate::AppState>();
                match crate::materialize_remote_conversation(
                    &self.app,
                    &app_state,
                    Arc::clone(self),
                    server_cid,
                    &agent.backend_kind,
                    &agent.workspace_roots,
                )
                .await
                {
                    Ok(local_cid) => local_cid,
                    Err(err) => {
                        tracing::warn!(
                            "Skipping remote agent {}: failed to materialize server conversation {}: {}",
                            agent.agent_id,
                            server_cid,
                            err
                        );
                        continue;
                    }
                }
            };
            // Only register ownership after successful materialization
            fresh_agent_ids.insert(agent.agent_id.clone());
            fresh_agent_to_server_cid.insert(agent.agent_id.clone(), server_cid);
            agent.conversation_id = local_cid;
            agent.workspace_roots = self.normalize_workspace_roots(&agent.workspace_roots);
            out.push(agent);
        }

        // Reconcile ownership from the server's authoritative list so stale
        // terminated agents are removed locally.
        *self.remote_agent_ids.lock().await = fresh_agent_ids;
        *self.agent_to_server_conversation.lock().await = fresh_agent_to_server_cid;
        Ok(out)
    }

    async fn emit_state(&self, new_state: ConnectionState) {
        *self.state.lock().await = new_state.clone();
        let _ = self.app.emit(
            "tyde-server-connection-state",
            ConnectionStateEvent {
                host_id: self.host_id.clone(),
                state: new_state,
            },
        );
    }
}

async fn close_tunnel(conn: &Arc<TydeServerConnection>) {
    if let Some(mut child) = conn.tunnel_child.lock().await.take() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    if let Some(path) = conn.tunnel_socket_path.lock().await.take() {
        let _ = std::fs::remove_file(path);
    }
}

/// Standalone implementation of connection setup. Lives outside the impl block
/// so it takes `Arc<Self>` by value rather than `self: &Arc<Self>`, making the
/// resulting future Send (required by tokio::spawn in the reconnect loop).
fn establish_connection_impl(
    this: Arc<TydeServerConnection>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move {
        match establish_connection_inner(Arc::clone(&this)).await {
            Ok(()) => Ok(()),
            Err(err) => {
                close_tunnel(&this).await;
                Err(err)
            }
        }
    })
}

async fn establish_connection_inner(this: Arc<TydeServerConnection>) -> Result<(), String> {
    close_tunnel(&this).await;

    {
        *this.state.lock().await = ConnectionState::Connecting;
        let _ = this.app.emit(
            "tyde-server-connection-state",
            ConnectionStateEvent {
                host_id: this.host_id.clone(),
                state: ConnectionState::Connecting,
            },
        );
    }

    let (child, local_socket_path, stream) =
        open_ssh_unix_socket_tunnel(&this.ssh_host, &this.remote_socket_path).await?;
    *this.tunnel_child.lock().await = Some(child);
    *this.tunnel_socket_path.lock().await = Some(local_socket_path);

    let (child_stdout, child_stdin) = stream.into_split();

    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<String>(256);

    let writer_handle = tokio::spawn(async move {
        let mut stdin = child_stdin;
        while let Some(line) = stdin_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(b"\n").await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });

    *this.stdin_tx.lock().await = Some(stdin_tx.clone());
    *this.writer_handle.lock().await = Some(writer_handle);

    let handshake = ClientFrame::Handshake {
        req_id: 0,
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION.to_string(),
        last_agent_event_seq: this.last_agent_event_seq.load(Ordering::Relaxed),
        last_chat_event_seqs: this
            .last_chat_event_seqs
            .lock()
            .await
            .iter()
            .map(|(cid, seq)| (cid.to_string(), *seq))
            .collect(),
    };
    let json = serde_json::to_string(&handshake)
        .map_err(|e| format!("Failed to serialize handshake: {e}"))?;
    stdin_tx
        .send(json)
        .await
        .map_err(|_| "Failed to send handshake")?;

    let mut reader = BufReader::new(child_stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("Failed to read handshake response: {e}"))?;

    let response: ServerFrame = serde_json::from_str(line.trim())
        .map_err(|e| format!("Invalid handshake response: {e}"))?;

    match response {
        ServerFrame::Result { data, .. } => {
            let result: HandshakeResult = serde_json::from_value(data)
                .map_err(|e| format!("Invalid handshake result: {e}"))?;
            if result.protocol_version != PROTOCOL_VERSION {
                return Err(format!(
                    "Protocol mismatch for host {}: local={} remote={}",
                    this.ssh_host, PROTOCOL_VERSION, result.protocol_version
                ));
            }
            if result.tyde_version != TYDE_VERSION {
                tracing::warn!(
                    "Tyde client/server version mismatch for host {}: local={} remote={}",
                    this.ssh_host,
                    TYDE_VERSION,
                    result.tyde_version
                );
                let _ = this.app.emit(
                    "tyde-server-version-warning",
                    VersionWarningEvent {
                        host_id: this.host_id.clone(),
                        host: this.ssh_host.clone(),
                        local_version: TYDE_VERSION.to_string(),
                        remote_version: result.tyde_version.clone(),
                    },
                );
            }
            let instance_changed = {
                let mut current = this.remote_instance_id.lock().await;
                let changed = match (&*current, &result.instance_id) {
                    (Some(prev), Some(next)) => prev != next,
                    (Some(_), None) => true,
                    _ => false,
                };
                *current = result.instance_id.clone();
                changed
            };
            if instance_changed {
                tracing::warn!(
                    "Remote Tyde instance changed for host {} — clearing cached mappings",
                    this.ssh_host
                );
                this.server_to_local_conv.lock().await.clear();
                this.remote_agent_ids.lock().await.clear();
                this.agent_to_server_conversation.lock().await.clear();
                this.last_agent_event_seq.store(0, Ordering::Relaxed);
                this.last_chat_event_seqs.lock().await.clear();
            }

            let app_state = this.app.state::<crate::AppState>();
            // 1. Materialize conversations first
            for conv in &result.conversations {
                if let Err(e) = crate::materialize_remote_conversation(
                    &this.app,
                    &app_state,
                    this.clone(),
                    conv.conversation_id,
                    &conv.backend_kind,
                    &conv.workspace_roots,
                )
                .await
                {
                    tracing::error!(
                        "Failed to materialize remote conversation {}: {}",
                        conv.conversation_id,
                        e
                    );
                }
            }

            // 2. Then sync agents
            for agent in &result.agents {
                this.remote_agent_ids
                    .lock()
                    .await
                    .insert(agent.agent_id.clone());
                this.agent_to_server_conversation
                    .lock()
                    .await
                    .insert(agent.agent_id.clone(), agent.conversation_id);
                let mut local_cid = this.translate_conversation_id(agent.conversation_id).await;
                if local_cid.is_none() {
                    let app_state = this.app.state::<crate::AppState>();
                    if let Err(err) = crate::materialize_remote_conversation(
                        &this.app,
                        &app_state,
                        Arc::clone(&this),
                        agent.conversation_id,
                        &agent.backend_kind,
                        &agent.workspace_roots,
                    )
                    .await
                    {
                        tracing::warn!(
                            "Failed to materialize remote conversation {} from agent {}: {}",
                            agent.conversation_id,
                            agent.agent_id,
                            err
                        );
                    } else {
                        local_cid = this.translate_conversation_id(agent.conversation_id).await;
                    }
                }
                if let Some(local_cid) = local_cid {
                    let mut translated = agent.clone();
                    translated.conversation_id = local_cid;
                    translated.workspace_roots =
                        this.normalize_workspace_roots(&translated.workspace_roots);
                    let _ = this.app.emit("agent-changed", translated);
                } else {
                    tracing::warn!(
                        "Skipping agent {} emit: no local mapping for server conversation {}",
                        agent.agent_id,
                        agent.conversation_id
                    );
                }
            }
            *this.remote_conversations.lock().await = result.conversations;
            *this.remote_session_records.lock().await = result.session_records;
            *this.remote_projects.lock().await = result.projects;
        }
        ServerFrame::Error { error, .. } => {
            return Err(format!("Handshake rejected: {error}"));
        }
        _ => {
            return Err("Unexpected handshake response".into());
        }
    }

    let conn_for_reader = Arc::clone(&this);
    let reader_handle = tokio::spawn(async move {
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ServerFrame>(trimmed) {
                        Ok(frame) => {
                            conn_for_reader.handle_server_frame(frame).await;
                        }
                        Err(err) => {
                            let preview: String = trimmed.chars().take(240).collect();
                            let truncated = if trimmed.chars().count() > 240 {
                                format!("{preview}…")
                            } else {
                                preview
                            };
                            tracing::warn!(
                                host_id = %conn_for_reader.host_id,
                                host = %conn_for_reader.ssh_host,
                                error = %err,
                                line = %truncated,
                                "Failed to parse remote server frame"
                            );
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Drain all pending requests with errors so callers don't hang
        {
            let mut pending = conn_for_reader.pending_requests.lock().await;
            for (_, req) in pending.drain() {
                let _ = req.tx.send(Err("Connection lost".to_string()));
            }
        }
        close_tunnel(&conn_for_reader).await;
        {
            *conn_for_reader.state.lock().await = ConnectionState::Disconnected {
                reason: "SSH connection lost".into(),
            };
            let _ = conn_for_reader.app.emit(
                "tyde-server-connection-state",
                ConnectionStateEvent {
                    host_id: conn_for_reader.host_id.clone(),
                    state: ConnectionState::Disconnected {
                        reason: "SSH connection lost".into(),
                    },
                },
            );
        }
        reconnect_loop(conn_for_reader).await;
    });

    *this.reader_handle.lock().await = Some(reader_handle);
    {
        *this.state.lock().await = ConnectionState::Connected;
        let _ = this.app.emit(
            "tyde-server-connection-state",
            ConnectionStateEvent {
                host_id: this.host_id.clone(),
                state: ConnectionState::Connected,
            },
        );
    }
    Ok(())
}

/// Standalone reconnection loop — avoids recursive async methods that can't
/// prove Send to the compiler.
async fn reconnect_loop(conn: Arc<TydeServerConnection>) {
    const MAX_ATTEMPTS: u32 = 10;
    const MAX_DELAY_SECS: u64 = 30;

    for attempt in 1..=MAX_ATTEMPTS {
        {
            let s = ConnectionState::Reconnecting { attempt };
            *conn.state.lock().await = s.clone();
            let _ = conn.app.emit(
                "tyde-server-connection-state",
                ConnectionStateEvent {
                    host_id: conn.host_id.clone(),
                    state: s,
                },
            );
        }
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(MAX_DELAY_SECS));
        tokio::time::sleep(delay).await;

        match establish_connection_impl(Arc::clone(&conn)).await {
            Ok(()) => {
                tracing::info!("Reconnected to remote Tyde on attempt {attempt}");
                return;
            }
            Err(e) => {
                tracing::warn!("Reconnection attempt {attempt} failed: {e}");
            }
        }
    }

    {
        let s = ConnectionState::Disconnected {
            reason: format!("Failed to reconnect after {MAX_ATTEMPTS} attempts"),
        };
        *conn.state.lock().await = s.clone();
        let _ = conn.app.emit(
            "tyde-server-connection-state",
            ConnectionStateEvent {
                host_id: conn.host_id.clone(),
                state: s,
            },
        );
    }
}

impl Drop for TydeServerConnection {
    fn drop(&mut self) {
        if let Ok(mut child_guard) = self.tunnel_child.try_lock() {
            if let Some(mut child) = child_guard.take() {
                let _ = child.start_kill();
            }
        }
        if let Ok(mut path_guard) = self.tunnel_socket_path.try_lock() {
            if let Some(path) = path_guard.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
