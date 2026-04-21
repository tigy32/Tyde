use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, OperationCancelledData,
    RetryAttemptData, SessionId, StreamEndData, StreamStartData, StreamTextDeltaData, TaskList,
    ToolExecutionCompletedData, ToolRequest,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, EventStream, StartupMcpServer,
    StartupMcpTransport, empty_session_settings_schema, render_combined_spawn_instructions,
    setup::resolve_tycode_binary_path,
};
use crate::process_env;

const BACKEND_INPUT_BUFFER: usize = 64;
const BACKEND_EVENT_BUFFER: usize = 256;

fn subprocess_bin() -> Result<String, String> {
    resolve_tycode_binary_path().ok_or_else(|| "tycode-subprocess not found".to_string())
}

pub struct TycodeBackend {
    input_tx: mpsc::Sender<AgentInput>,
    interrupt_tx: mpsc::Sender<()>,
    shutdown_tx: mpsc::Sender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

enum TycodeStdinCommand {
    Json(Value),
    Cancel,
}

struct TempWorkspaceRoot {
    path: PathBuf,
}

impl TempWorkspaceRoot {
    fn new(prefix: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).map_err(|err| {
            format!(
                "Failed to create temporary workspace {}: {err}",
                path.display()
            )
        })?;
        Ok(Self { path })
    }
}

impl Drop for TempWorkspaceRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_text_file(path: &PathBuf, body: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("Failed to create directory {}: {err}", parent.display()))?;
    fs::write(path, body).map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

fn materialize_tycode_customization(
    config: &BackendSpawnConfig,
) -> Result<Option<TempWorkspaceRoot>, String> {
    let steering = render_combined_spawn_instructions(&config.resolved_spawn_config);
    if steering.is_none() && config.resolved_spawn_config.skills.is_empty() {
        return Ok(None);
    }
    let root = TempWorkspaceRoot::new("tyde-tycode-customization")?;
    if let Some(steering) = steering {
        write_text_file(
            &root.path.join(".tycode").join("tyde_steering.md"),
            &steering,
        )?;
    }
    for skill in &config.resolved_spawn_config.skills {
        write_text_file(
            &root
                .path
                .join(".tycode")
                .join("skills")
                .join(&skill.name)
                .join("SKILL.md"),
            &skill.body,
        )?;
    }
    Ok(Some(root))
}

fn backend_user_message(content: String, images: Option<Vec<protocol::ImageData>>) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        timestamp: unix_now_ms(),
        sender: MessageSender::User,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images,
    })
}

impl Backend for TycodeBackend {
    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Tycode)
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        let initial_images = initial_input.images;
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let workspace_roots = if workspace_roots.is_empty() {
            vec!["/tmp".to_string()]
        } else {
            workspace_roots
        };
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode customization: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let known_session_ids = known_tycode_session_ids();
            let subprocess_bin = match subprocess_bin() {
                Ok(path) => path,
                Err(err) => {
                    tracing::error!("{err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut command = Command::new(&subprocess_bin);
            command.arg("--workspace-roots").arg(&roots_json);
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                command.env("PATH", path);
            }
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn tycode-subprocess: {err}")));
                    return;
                }
            };

            let stdin = match child.stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin");
                    let _ =
                        ready_tx.send(Err("Failed to capture tycode-subprocess stdin".to_string()));
                    return;
                }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stdout".to_string()));
                    return;
                }
            };

            // Spawn a task to forward follow-up messages to stdin
            let (stdin_tx, mut stdin_rx) =
                mpsc::channel::<TycodeStdinCommand>(BACKEND_INPUT_BUFFER);
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            if events_tx
                .send(backend_user_message(
                    initial_message.clone(),
                    initial_images.clone(),
                ))
                .await
                .is_err()
            {
                let _ = ready_tx.send(Err("Tycode event stream closed during spawn".to_string()));
                return;
            }
            if stdin_tx
                .send(TycodeStdinCommand::Json(
                    serde_json::json!({ "UserInput": initial_message }),
                ))
                .await
                .is_err()
            {
                let _ = ready_tx.send(Err("Tycode stdin writer closed during spawn".to_string()));
                return;
            }

            // Forward AgentInput to the stdin writer
            let stdin_tx2 = stdin_tx.clone();
            let events_tx2 = events_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            let images = payload.images;
                            if events_tx2
                                .send(backend_user_message(message.clone(), images))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(_) => {}
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt
                        .send(TycodeStdinCommand::Cancel)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            // Read stdout line by line — the subprocess emits ChatEvent JSON directly
            let mut lines = BufReader::new(stdout).lines();
            let mut stream_open = false;
            let mut accumulated_text = String::new();
            let mut ready_tx = Some(ready_tx);
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    shutdown = shutdown_rx.recv() => {
                        if shutdown.is_some() {
                            let _ = child.kill().await;
                        }
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse tycode-subprocess event: {err} — line: {trimmed}"
                        );
                        continue;
                    }
                };

                if session_id_task
                    .lock()
                    .expect("tycode session_id mutex poisoned")
                    .is_none()
                    && let Some(session) = tycode_session_started(&value)
                {
                    *session_id_task
                        .lock()
                        .expect("tycode session_id mutex poisoned") = Some(session);
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Ok(()));
                    }
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    if session_id_task
                        .lock()
                        .expect("tycode session_id mutex poisoned")
                        .is_none()
                        && let Some(discovered) = discover_new_tycode_session(&known_session_ids)
                    {
                        *session_id_task
                            .lock()
                            .expect("tycode session_id mutex poisoned") = Some(discovered);
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                    }
                    continue;
                }

                for event in events {
                    let mut outbound = Vec::with_capacity(2);

                    match &event {
                        ChatEvent::StreamStart(StreamStartData { .. }) => {
                            stream_open = true;
                            accumulated_text.clear();
                        }
                        ChatEvent::StreamDelta(StreamTextDeltaData { message_id, text }) => {
                            if !stream_open {
                                outbound
                                    .push(synthetic_tycode_stream_start(message_id.clone(), None));
                                stream_open = true;
                                accumulated_text.clear();
                            }
                            accumulated_text.push_str(text);
                        }
                        ChatEvent::StreamEnd(StreamEndData { message }) => {
                            if !stream_open {
                                let model =
                                    message.model_info.as_ref().map(|info| info.model.clone());
                                outbound.push(synthetic_tycode_stream_start(
                                    Some(format!("tycode-msg-{}", message.timestamp)),
                                    model,
                                ));
                            }
                            stream_open = false;
                        }
                        _ => {}
                    }

                    outbound.push(event);
                    for outbound_event in outbound {
                        if events_tx.send(outbound_event).await.is_err() {
                            break;
                        }
                    }
                    if events_tx.is_closed() {
                        break;
                    }
                }

                if session_id_task
                    .lock()
                    .expect("tycode session_id mutex poisoned")
                    .is_none()
                    && let Some(discovered) = discover_new_tycode_session(&known_session_ids)
                {
                    *session_id_task
                        .lock()
                        .expect("tycode session_id mutex poisoned") = Some(discovered);
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Ok(()));
                    }
                }
            }

            // Some tycode builds terminate without emitting StreamEnd. Synthesize
            // one so downstream callers don't hang waiting for end-of-turn.
            if stream_open {
                let _ = events_tx
                    .send(ChatEvent::StreamEnd(StreamEndData {
                        message: ChatMessage {
                            timestamp: unix_now_ms(),
                            sender: MessageSender::Assistant {
                                agent: "tycode".to_string(),
                            },
                            content: accumulated_text,
                            reasoning: None,
                            tool_calls: Vec::new(),
                            model_info: None,
                            token_usage: None,
                            context_breakdown: None,
                            images: None,
                        },
                    }))
                    .await;
            }

            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(
                    "Tycode process exited before reporting a session_id".to_string()
                ));
            }
        });

        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Tycode spawn initialization task ended early".to_string()),
        }

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
                session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let workspace_roots = if workspace_roots.is_empty() {
            vec!["/tmp".to_string()]
        } else {
            workspace_roots
        };
        let known_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode resume customization: {err}");
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let subprocess_bin = match subprocess_bin() {
                Ok(path) => path,
                Err(err) => {
                    tracing::error!("{err}");
                    return;
                }
            };
            let mut command = Command::new(&subprocess_bin);
            command.arg("--workspace-roots").arg(&roots_json);
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                command.env("PATH", path);
            }
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess for resume: {err}");
                    return;
                }
            };

            let stdin = match child.stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin for resume");
                    return;
                }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout for resume");
                    return;
                }
            };

            let (stdin_tx, mut stdin_rx) =
                mpsc::channel::<TycodeStdinCommand>(BACKEND_INPUT_BUFFER);
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            if stdin_tx
                .send(TycodeStdinCommand::Json(serde_json::json!({
                    "ResumeSession": { "session_id": session_id.0 }
                })))
                .await
                .is_err()
            {
                return;
            }

            let stdin_tx2 = stdin_tx.clone();
            let events_tx2 = events_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            let images = payload.images;
                            if events_tx2
                                .send(backend_user_message(message.clone(), images))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(_) => {}
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt
                        .send(TycodeStdinCommand::Cancel)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            let mut lines = BufReader::new(stdout).lines();
            let mut stream_open = false;
            let mut accumulated_text = String::new();
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    shutdown = shutdown_rx.recv() => {
                        if shutdown.is_some() {
                            let _ = child.kill().await;
                        }
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse tycode-subprocess resume event: {err} — line: {trimmed}"
                        );
                        continue;
                    }
                };

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in events {
                    match &event {
                        ChatEvent::StreamStart(StreamStartData { .. }) => {
                            stream_open = true;
                            accumulated_text.clear();
                        }
                        ChatEvent::StreamDelta(StreamTextDeltaData { text, .. }) => {
                            if stream_open {
                                accumulated_text.push_str(text);
                            }
                        }
                        ChatEvent::StreamEnd(_) => {
                            stream_open = false;
                        }
                        _ => {}
                    }

                    if events_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }

            if stream_open {
                let _ = events_tx
                    .send(ChatEvent::StreamEnd(StreamEndData {
                        message: ChatMessage {
                            timestamp: unix_now_ms(),
                            sender: MessageSender::Assistant {
                                agent: "tycode".to_string(),
                            },
                            content: accumulated_text,
                            reasoning: None,
                            tool_calls: Vec::new(),
                            model_info: None,
                            token_usage: None,
                            context_breakdown: None,
                            images: None,
                        },
                    }))
                    .await;
            }
        });

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
                session_id: known_session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        list_tycode_sessions()
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("tycode session_id mutex poisoned")
            .clone()
            .expect("tycode session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).await.is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).await.is_ok()
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(()).await;
    }
}

async fn write_command(stdin: &mut tokio::process::ChildStdin, command: &Value) -> bool {
    let line = match serde_json::to_string(command) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("Failed to serialize tycode command: {err}");
            return false;
        }
    };

    if let Err(err) = stdin.write_all(line.as_bytes()).await {
        tracing::error!("Failed to write to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.write_all(b"\n").await {
        tracing::error!("Failed to write newline to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess stdin: {err}");
        return false;
    }
    true
}

async fn write_cancel(stdin: &mut tokio::process::ChildStdin) -> bool {
    if let Err(err) = stdin.write_all(b"CANCEL\n").await {
        tracing::error!("Failed to write cancel to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess cancel: {err}");
        return false;
    }
    true
}

fn tycode_sessions_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory".to_string())?;
    Ok(PathBuf::from(home).join(".tycode").join("sessions"))
}

fn known_tycode_session_ids() -> Vec<String> {
    list_tycode_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|session| session.id.0)
        .collect()
}

fn discover_new_tycode_session(known_session_ids: &[String]) -> Option<SessionId> {
    let known: std::collections::HashSet<_> = known_session_ids.iter().collect();
    let mut sessions = list_tycode_sessions().ok()?;
    sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    sessions
        .into_iter()
        .find(|session| !known.contains(&session.id.0))
        .map(|session| session.id)
}

fn build_tycode_mcp_servers_json(startup_mcp_servers: &[StartupMcpServer]) -> Option<String> {
    if startup_mcp_servers.is_empty() {
        return None;
    }

    let mut servers = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let config = match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut config = serde_json::Map::new();
                config.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    config.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .expect("HashMap<String, String> is always serializable"),
                    );
                }
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    config.insert(
                        "bearer_token_env_var".to_string(),
                        Value::String(env_var.to_string()),
                    );
                }
                Value::Object(config)
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                serde_json::json!({
                    "command": trimmed_command,
                    "args": args,
                    "env": env,
                })
            }
        };
        servers.insert(name.to_string(), config);
    }

    if servers.is_empty() {
        return None;
    }

    Some(serde_json::json!({ "mcpServers": servers }).to_string())
}

fn tycode_session_started(value: &Value) -> Option<SessionId> {
    if value.get("kind").and_then(Value::as_str) != Some("SessionStarted") {
        return None;
    }

    value
        .get("data")
        .and_then(|data| data.get("session_id"))
        .and_then(Value::as_str)
        .map(|session_id| SessionId(session_id.to_string()))
}

fn synthetic_tycode_stream_start(message_id: Option<String>, model: Option<String>) -> ChatEvent {
    ChatEvent::StreamStart(StreamStartData {
        message_id,
        agent: "tycode".to_string(),
        model,
    })
}

fn list_tycode_sessions() -> Result<Vec<BackendSession>, String> {
    let sessions_dir = tycode_sessions_dir()?;
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "Failed to read Tycode sessions directory {}: {err}",
                sessions_dir.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session {:?}: {err}", path);
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&json) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("Skipping unparseable Tycode session {:?}: {err}", path);
                continue;
            }
        };

        let Some(id) = value.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let created_at_ms = value.get("created_at").and_then(Value::as_u64);
        let updated_at_ms = value.get("last_modified").and_then(Value::as_u64);
        let title = extract_tycode_title(&value);

        sessions.push(BackendSession {
            id: SessionId(id),
            backend_kind: BackendKind::Tycode,
            workspace_roots: Vec::new(),
            title,
            token_count: None,
            created_at_ms,
            updated_at_ms,
            resumable: true,
        });
    }

    sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    Ok(sessions)
}

fn extract_tycode_title(value: &Value) -> Option<String> {
    let messages = value.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("User") {
            continue;
        }
        if let Some(text) = message
            .get("content")
            .and_then(|content| content.get("blocks"))
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(80).collect());
            }
        }
    }
    None
}

fn map_tycode_value_to_chat_events(value: &Value) -> Vec<ChatEvent> {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(value.clone()) {
        match event {
            ChatEvent::MessageAdded(_) => {}
            _ => return vec![event],
        }
    }

    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = value.get("data").cloned().unwrap_or(Value::Null);

    match kind {
        "MessageAdded" => map_tycode_message_added(&data),
        "TaskUpdate" => serde_json::from_value::<TaskList>(data)
            .map(ChatEvent::TaskUpdate)
            .into_iter()
            .collect(),
        "ToolRequest" => serde_json::from_value::<ToolRequest>(data)
            .map(ChatEvent::ToolRequest)
            .into_iter()
            .collect(),
        "ToolExecutionCompleted" => serde_json::from_value::<ToolExecutionCompletedData>(data)
            .map(ChatEvent::ToolExecutionCompleted)
            .into_iter()
            .collect(),
        "OperationCancelled" => serde_json::from_value::<OperationCancelledData>(data)
            .map(ChatEvent::OperationCancelled)
            .into_iter()
            .collect(),
        "RetryAttempt" => serde_json::from_value::<RetryAttemptData>(data)
            .map(ChatEvent::RetryAttempt)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn map_tycode_message_added(data: &Value) -> Vec<ChatEvent> {
    let Some(sender) = parse_tycode_sender(data.get("sender")) else {
        return Vec::new();
    };
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let timestamp = data
        .get("timestamp")
        .and_then(Value::as_u64)
        .unwrap_or_else(unix_now_ms);

    let message = ChatMessage {
        timestamp,
        sender: sender.clone(),
        content: content.clone(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    };

    match sender {
        MessageSender::Assistant { agent } => {
            let message_id = Some(format!("tycode-msg-{timestamp}"));
            let mut events = vec![ChatEvent::StreamStart(StreamStartData {
                message_id: message_id.clone(),
                agent,
                model: None,
            })];
            if !content.is_empty() {
                events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: message_id.clone(),
                    text: content,
                }));
            }
            events.push(ChatEvent::StreamEnd(StreamEndData { message }));
            events
        }
        _ => vec![ChatEvent::MessageAdded(message)],
    }
}

fn parse_tycode_sender(sender: Option<&Value>) -> Option<MessageSender> {
    let sender = sender?;
    if let Some(name) = sender.as_str() {
        return match name {
            "User" => Some(MessageSender::User),
            "System" => Some(MessageSender::System),
            "Warning" => Some(MessageSender::Warning),
            "Error" => Some(MessageSender::Error),
            _ => None,
        };
    }

    let assistant = sender.get("Assistant")?;
    let agent = assistant
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("tycode")
        .to_string();
    Some(MessageSender::Assistant { agent })
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn build_tycode_mcp_servers_json_supports_http_servers() {
        let json = build_tycode_mcp_servers_json(&[StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url: "http://127.0.0.1:4123/mcp".to_string(),
                headers: HashMap::from([(
                    "x-tyde-debug-repo-root".to_string(),
                    "/tmp/project".to_string(),
                )]),
                bearer_token_env_var: None,
            },
        }])
        .expect("HTTP MCP config should serialize");
        let value: Value = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(
            value["mcpServers"]["tyde-debug"]["url"],
            Value::String("http://127.0.0.1:4123/mcp".to_string())
        );
        assert_eq!(
            value["mcpServers"]["tyde-debug"]["headers"]["x-tyde-debug-repo-root"],
            Value::String("/tmp/project".to_string())
        );
    }
}
