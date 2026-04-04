#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(unix)]
use serde::Deserialize;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use tauri::async_runtime::JoinHandle;
#[cfg(unix)]
use tauri::Manager;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};

use crate::chat_buffer::ChatEventBuffer;
#[cfg(unix)]
use crate::protocol::{
    ClientFrame, ConversationSnapshot, HandshakeResult, ServerFrame, PROTOCOL_VERSION,
};
#[cfg(not(unix))]
use crate::protocol::ServerFrame;

/// Manages the UDS listener that accepts remote Tyde clients.
/// Started when "Allow remote control" is enabled in settings.
pub struct RemoteControlServer {
    socket_path: PathBuf,
    #[cfg(unix)]
    instance_id: String,
    #[cfg(unix)]
    accept_task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    pub event_broadcast: broadcast::Sender<ServerFrame>,
    pub chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    #[cfg(unix)]
    clients: Arc<Mutex<Vec<u64>>>,
}

#[cfg(unix)]
impl RemoteControlServer {
    pub fn start(app: tauri::AppHandle) -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("Cannot determine home directory")?;
        let socket_dir = home.join(".tyde");
        std::fs::create_dir_all(&socket_dir)
            .map_err(|e| format!("Failed to create ~/.tyde: {e}"))?;

        let socket_path = socket_dir.join("tyde.sock");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .map_err(|e| format!("Failed to remove stale socket: {e}"))?;
        }

        let (event_tx, _) = broadcast::channel::<ServerFrame>(1024);
        let chat_buffer = Arc::new(parking_lot::Mutex::new(ChatEventBuffer::new()));
        let clients: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        let server = Self {
            socket_path,
            instance_id: uuid::Uuid::new_v4().to_string(),
            accept_task: parking_lot::Mutex::new(None),
            event_broadcast: event_tx,
            chat_buffer,
            clients,
        };

        server.start_listening(app)?;
        Ok(server)
    }

    pub fn start_listening(&self, app: tauri::AppHandle) -> Result<(), String> {
        if self.is_running() {
            return Ok(());
        }

        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| format!("Failed to remove stale socket: {e}"))?;
        }

        let listener = StdUnixListener::bind(&self.socket_path)
            .map_err(|e| format!("Failed to bind UDS at {}: {e}", self.socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set UDS listener nonblocking: {e}"))?;

        tracing::info!("Remote control listening on {}", self.socket_path.display());

        let socket_path = self.socket_path.clone();
        let instance_id = self.instance_id.clone();
        let event_broadcast = self.event_broadcast.clone();
        let chat_buffer = self.chat_buffer.clone();
        let clients = self.clients.clone();
        let accept_handle = tauri::async_runtime::spawn(async move {
            let listener = match UnixListener::from_std(listener) {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::warn!("Remote control server failed to create async listener: {err}");
                    let _ = std::fs::remove_file(socket_path);
                    return;
                }
            };
            accept_loop(
                app,
                listener,
                instance_id,
                event_broadcast,
                chat_buffer,
                clients,
            )
            .await;
        });
        *self.accept_task.lock() = Some(accept_handle);
        Ok(())
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub async fn connected_client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    pub fn shutdown(&self) {
        if let Some(handle) = self.accept_task.lock().take() {
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!("Remote control server stopped");
    }

    pub fn is_running(&self) -> bool {
        self.accept_task
            .lock()
            .as_ref()
            .is_some_and(|h| !h.inner().is_finished())
            && self.socket_path.exists()
    }
}

#[cfg(not(unix))]
impl RemoteControlServer {
    pub fn start(_app: tauri::AppHandle) -> Result<Self, String> {
        Err("Remote control requires Unix domain sockets (not available on this platform)".into())
    }

    pub fn start_listening(&self, _app: tauri::AppHandle) -> Result<(), String> {
        Err("Remote control requires Unix domain sockets (not available on this platform)".into())
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub async fn connected_client_count(&self) -> usize {
        0
    }

    pub fn shutdown(&self) {}

    pub fn is_running(&self) -> bool {
        false
    }
}

#[cfg(unix)]
impl Drop for RemoteControlServer {
    fn drop(&mut self) {
        if let Some(handle) = self.accept_task.get_mut().take() {
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn accept_loop(
    app: tauri::AppHandle,
    listener: UnixListener,
    instance_id: String,
    event_tx: broadcast::Sender<ServerFrame>,
    chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    clients: Arc<Mutex<Vec<u64>>>,
) {
    let next_id = AtomicU64::new(1);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("UDS accept error: {e}");
                continue;
            }
        };

        let cid = next_id.fetch_add(1, Ordering::Relaxed);
        tracing::info!("Remote client {cid} connected");
        clients.lock().await.push(cid);

        let app = app.clone();
        let event_rx = event_tx.subscribe();
        let chat_buffer = chat_buffer.clone();
        let clients = clients.clone();
        let instance_id_for_client = instance_id.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(
                app,
                stream,
                cid,
                event_rx,
                chat_buffer,
                instance_id_for_client,
            )
            .await
            {
                tracing::warn!("Remote client {cid} error: {e}");
            }
            tracing::info!("Remote client {cid} disconnected");
            clients.lock().await.retain(|id| *id != cid);
        });
    }
}

// ---------------------------------------------------------------------------
// Per-client handler
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn handle_client(
    app: tauri::AppHandle,
    stream: tokio::net::UnixStream,
    client_id: u64,
    mut event_rx: broadcast::Receiver<ServerFrame>,
    chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    instance_id: String,
) -> Result<(), String> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let writer = Arc::new(Mutex::new(write_half));

    // --- Handshake -----------------------------------------------------------
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("Read handshake: {e}"))?;

    let handshake: ClientFrame =
        serde_json::from_str(line.trim()).map_err(|e| format!("Invalid handshake: {e}"))?;

    let (req_id, last_agent_seq, last_chat_seqs_raw) = match handshake {
        ClientFrame::Handshake {
            req_id,
            protocol_version,
            last_agent_event_seq,
            last_chat_event_seqs,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                send(
                    &writer,
                    &ServerFrame::Error {
                        req_id,
                        error: format!(
                            "Protocol mismatch: client={protocol_version} server={PROTOCOL_VERSION}"
                        ),
                    },
                )
                .await?;
                return Err("Protocol version mismatch".into());
            }
            (req_id, last_agent_event_seq, last_chat_event_seqs)
        }
        _ => return Err("First message must be Handshake".into()),
    };
    let mut last_chat_seqs = HashMap::new();
    for (raw_conversation_id, seq) in last_chat_seqs_raw {
        match raw_conversation_id.parse::<u64>() {
            Ok(conversation_id) => {
                last_chat_seqs.insert(conversation_id, seq);
            }
            Err(_) => {
                tracing::warn!(
                    "Client {client_id}: ignoring invalid chat replay cursor key '{}'",
                    raw_conversation_id
                );
            }
        }
    }

    let state = app.state::<crate::AppState>();

    let agents = state.agent_runtime.lock().await.list_agents();
    let conversations = {
        let mgr = state.manager.lock().await;
        let buf = chat_buffer.lock();
        mgr.active_ids()
            .into_iter()
            .map(|id| ConversationSnapshot {
                conversation_id: id,
                // active_ids() returns keys from the same map, so these lookups
                // cannot fail while we hold the lock.
                backend_kind: mgr.backend_kind(id).expect("active conversation has backend_kind").as_str().to_string(),
                workspace_roots: mgr
                    .workspace_roots(id)
                    .expect("active conversation has workspace_roots")
                    .to_vec(),
                chat_event_seq: buf.latest_seq_for_conversation(id),
            })
            .collect()
    };

    let session_records = state.session_store.lock().list()
        .map_err(|e| format!("Failed to read session store: {e}"))?;


    send(
        &writer,
        &ServerFrame::Result {
            req_id,
            data: serde_json::to_value(HandshakeResult {
                protocol_version: PROTOCOL_VERSION,
                agents,
                conversations,
                instance_id: Some(instance_id),
                session_records,
            })
            .unwrap_or_default(),
        },
    )
    .await?;

    // --- Replay missed agent events ------------------------------------------
    {
        let runtime = state.agent_runtime.lock().await;
        let batch = runtime.events_since(last_agent_seq, 1000);
        for ev in &batch.events {
            if let Some(info) = runtime.get_agent(&ev.agent_id) {
                send(
                    &writer,
                    &ServerFrame::Event {
                        event: "agent-changed".into(),
                        seq: Some(ev.seq),
                        payload: serde_json::to_value(&info).unwrap_or_default(),
                    },
                )
                .await?;
            }
        }
    }

    // --- Replay missed chat events -------------------------------------------
    {
        let replay_frames: Vec<ServerFrame> = {
            let buf = chat_buffer.lock();
            buf.all_events_since(&last_chat_seqs)
                .into_iter()
                .map(|entry| ServerFrame::Event {
                    event: "chat-event".into(),
                    seq: Some(entry.seq),
                    payload: serde_json::json!({
                        "conversation_id": entry.conversation_id,
                        "event": entry.event,
                    }),
                })
                .collect()
        };
        for frame in &replay_frames {
            send(&writer, frame).await?;
        }
    }

    // --- Main loop: commands + event forwarding ------------------------------
    let w2 = writer.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(frame) => {
                    if send(&w2, &frame).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Client {client_id} lagged {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| format!("Read: {e}"))?;
        if n == 0 {
            break;
        }

        let frame: ClientFrame = match serde_json::from_str(buf.trim()) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Client {client_id}: bad frame: {e}");
                continue;
            }
        };

        let response = dispatch(&app, frame).await;
        send(&writer, &response).await?;
    }

    forwarder.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Command dispatch — routes Invoke frames to existing Tauri command logic
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn dispatch(app: &tauri::AppHandle, frame: ClientFrame) -> ServerFrame {
    match frame {
        ClientFrame::Handshake { req_id, .. } => ServerFrame::Error {
            req_id,
            error: "Handshake already completed".into(),
        },

        ClientFrame::Invoke {
            req_id,
            command,
            params,
        } => match dispatch_invoke(app, &command, params).await {
            Ok(data) => ServerFrame::Result { req_id, data },
            Err(error) => ServerFrame::Error { req_id, error },
        },
    }
}

#[cfg(unix)]
async fn dispatch_invoke(
    app: &tauri::AppHandle,
    command: &str,
    params: Value,
) -> Result<Value, String> {
    let state = app.state::<crate::AppState>();

    match command {
        "create_conversation" => {
            #[derive(Deserialize)]
            struct P {
                workspace_roots: Vec<String>,
                backend_kind: Option<String>,
                ephemeral: Option<bool>,
                agent_definition_id: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let resp = crate::create_conversation(
                app.clone(),
                app.state(),
                p.workspace_roots,
                p.backend_kind,
                p.ephemeral,
                p.agent_definition_id,
            )
            .await?;
            let backend_kind = {
                let mgr = state.manager.lock().await;
                mgr.backend_kind(resp.conversation_id)
                    .map(|k| k.as_str().to_string())
            };
            Ok(serde_json::json!({
                "conversation_id": resp.conversation_id,
                "session_id": resp.session_id,
                "backend_kind": backend_kind,
            }))
        }

        "send_message" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
                message: String,
                images: Option<Vec<crate::subprocess::ImageAttachment>>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::SendMessage {
                    message: p.message,
                    images: p.images,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "cancel_conversation" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::CancelConversation,
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "close_conversation" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::close_conversation(app.clone(), app.state(), p.conversation_id).await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "spawn_agent" => {
            #[derive(Deserialize)]
            struct P {
                workspace_roots: Vec<String>,
                prompt: String,
                backend_kind: Option<String>,
                parent_agent_id: Option<String>,
                name: String,
                ephemeral: Option<bool>,
                agent_definition_id: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let resp = crate::spawn_agent_internal(
                app,
                &state,
                crate::SpawnAgentRequest {
                    workspace_roots: p.workspace_roots,
                    prompt: p.prompt,
                    backend_kind: p.backend_kind,
                    parent_agent_id: p.parent_agent_id,
                    name: p.name,
                    ephemeral: p.ephemeral,
                    images: None,
                    agent_definition_id: p.agent_definition_id,
                },
            )
            .await?;
            let backend_kind = {
                let mgr = state.manager.lock().await;
                mgr.backend_kind(resp.conversation_id)
                    .map(|k| k.as_str().to_string())
            };
            Ok(serde_json::json!({
                "agent_id": resp.agent_id,
                "conversation_id": resp.conversation_id,
                "backend_kind": backend_kind,
            }))
        }

        "send_agent_message" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
                message: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::send_agent_message_internal(
                app,
                &state,
                crate::SendAgentMessageRequest {
                    agent_id: p.agent_id,
                    message: p.message,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "interrupt_agent" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::interrupt_agent_internal(
                app,
                &state,
                crate::AgentIdRequest {
                    agent_id: p.agent_id,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "terminate_agent" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::terminate_agent_internal(
                app,
                &state,
                crate::AgentIdRequest {
                    agent_id: p.agent_id,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "cancel_agent" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let result = crate::cancel_agent_internal(
                app,
                &state,
                crate::AgentIdRequest {
                    agent_id: p.agent_id,
                },
            )
            .await?;
            serde_json::to_value(&result).map_err(|e| e.to_string())
        }

        "list_agents" => {
            let agents = crate::list_agents_local_only_internal(&state).await;
            serde_json::to_value(&agents).map_err(|e| e.to_string())
        }

        "wait_for_agent" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let info = crate::wait_for_agent_internal(
                &state,
                crate::WaitForAgentRequest {
                    agent_id: p.agent_id,
                },
            )
            .await?;
            serde_json::to_value(&info).map_err(|e| e.to_string())
        }

        "collect_agent_result" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let result = crate::collect_agent_result_internal(
                &state,
                crate::AgentIdRequest {
                    agent_id: p.agent_id,
                },
            )
            .await?;
            serde_json::to_value(&result).map_err(|e| e.to_string())
        }

        "agent_events_since" => {
            #[derive(Deserialize)]
            struct P {
                since_seq: Option<u64>,
                limit: Option<usize>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let batch = crate::agent_events_since_internal(
                &state,
                crate::AgentEventsSinceRequest {
                    since_seq: p.since_seq,
                    limit: p.limit,
                },
            )
            .await?;
            serde_json::to_value(&batch).map_err(|e| e.to_string())
        }

        "rename_agent" => {
            #[derive(Deserialize)]
            struct P {
                agent_id: String,
                name: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::rename_agent_tauri_free(&state, p.agent_id, p.name).await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "list_agent_definitions" => {
            #[derive(Deserialize)]
            struct P {
                workspace_path: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let entries = crate::agent_defs_io::list_agent_definitions(p.workspace_path).await?;
            serde_json::to_value(&entries).map_err(|e| e.to_string())
        }

        "save_agent_definition" => {
            #[derive(Deserialize)]
            struct P {
                definition_json: String,
                scope: String,
                workspace_path: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::agent_defs_io::save_agent_definition(
                &p.definition_json,
                &p.scope,
                p.workspace_path,
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "delete_agent_definition" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                scope: String,
                workspace_path: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::agent_defs_io::delete_agent_definition(&p.id, &p.scope, p.workspace_path)
                .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        // Session-level commands forwarded to the backend via execute_conversation_command
        "get_settings" | "list_sessions" | "get_module_schemas" | "list_models"
        | "list_profiles" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let cmd = match command {
                "get_settings" => crate::backend::SessionCommand::GetSettings,
                "list_sessions" => crate::backend::SessionCommand::ListSessions,
                "get_module_schemas" => crate::backend::SessionCommand::GetModuleSchemas,
                "list_models" => crate::backend::SessionCommand::ListModels,
                "list_profiles" => crate::backend::SessionCommand::ListProfiles,
                _ => unreachable!(),
            };
            crate::execute_conversation_command(app, &state, p.conversation_id, cmd).await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "resume_session" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
                session_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::ResumeSession {
                    session_id: p.session_id,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "delete_session" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
                session_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::DeleteSession {
                    session_id: p.session_id,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "switch_profile" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
                profile_name: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::SwitchProfile {
                    profile_name: p.profile_name,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "update_settings" => {
            #[derive(Deserialize)]
            struct P {
                conversation_id: u64,
                settings: Value,
                persist: bool,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            crate::execute_conversation_command(
                app,
                &state,
                p.conversation_id,
                crate::backend::SessionCommand::UpdateSettings {
                    settings: p.settings,
                    persist: p.persist,
                },
            )
            .await?;
            Ok(serde_json::json!({"ok": true}))
        }

        "list_session_records" => {
            let mut store = state.session_store.lock();
            let records = store.list()?;
            serde_json::to_value(records).map_err(|e| e.to_string())
        }

        "rename_session" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                name: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let mut store = state.session_store.lock();
            store.set_user_alias(&p.id, &p.name)?;
            Ok(serde_json::json!({"ok": true}))
        }

        "set_session_alias" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                alias: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let mut store = state.session_store.lock();
            store.set_alias(&p.id, &p.alias)?;
            Ok(serde_json::json!({"ok": true}))
        }

        "delete_session_record" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| e.to_string())?;

            // Look up the record to get backend info before deleting
            let (backend_session_id, backend_kind_str, workspace_root) = {
                let mut store = state.session_store.lock();
                let record = store
                    .get(&p.id)
                    .ok_or_else(|| format!("Session record '{}' not found", p.id))?;
                (
                    record.backend_session_id.clone(),
                    record.backend_kind.clone(),
                    record.workspace_root.clone(),
                )
            };

            // Delete from backend via temp admin subprocess if there's a backend session
            if let Some(ref bsid) = backend_session_id {
                let roots: Vec<String> = workspace_root.iter().cloned().collect();
                let backend_kind = crate::resolve_requested_backend_kind(
                    &state,
                    Some(backend_kind_str),
                    &roots,
                )?;
                let launch_target =
                    crate::resolve_backend_launch_target(app, &roots, backend_kind).await?;
                let (session, _rx) = crate::backend::BackendSession::spawn_admin(
                    backend_kind,
                    &launch_target,
                    &roots,
                )
                .await?;
                let handle = session.command_handle();
                let result = handle
                    .execute(crate::backend::SessionCommand::DeleteSession {
                        session_id: bsid.clone(),
                    })
                    .await;
                session.shutdown().await;
                result?;
            }

            // Delete from store
            state.session_store.lock().delete(&p.id)?;
            Ok(serde_json::json!({"ok": true}))
        }

        "server_status" => Ok(serde_json::json!({
            "status": "running",
            "protocol_version": PROTOCOL_VERSION,
            "pid": std::process::id(),
        })),

        _ => Err(format!("Unknown command: {command}")),
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn send(
    writer: &Mutex<tokio::net::unix::OwnedWriteHalf>,
    frame: &ServerFrame,
) -> Result<(), String> {
    let mut line = serde_json::to_string(frame).map_err(|e| format!("Serialize: {e}"))?;
    line.push('\n');
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("Write: {e}"))?;
    w.flush().await.map_err(|e| format!("Flush: {e}"))?;
    Ok(())
}
