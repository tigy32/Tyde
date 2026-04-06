use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, Mutex};

use crate::backend_transport::{BackendLaunchTarget, BackendTransport};
use crate::claude::{ClaudeCommandHandle, ClaudeSession, SubAgentEmitter};
use crate::codex::{CodexCommandHandle, CodexSession};
use crate::gemini::{GeminiCommandHandle, GeminiSession};
use crate::kiro::{KiroCommandHandle, KiroSession};
use crate::remote::{shell_quote_arg, to_remote_uri};
use crate::subprocess::{ImageAttachment, SubprocessBridge};
use crate::tyde_server_conn::TydeServerConnection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Tycode,
    Codex,
    Claude,
    Kiro,
    Gemini,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tycode => "tycode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kiro => "kiro",
            Self::Gemini => "gemini",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tycode" => Ok(Self::Tycode),
            "codex" => Ok(Self::Codex),
            "claude" | "claude_code" => Ok(Self::Claude),
            "kiro" => Ok(Self::Kiro),
            "gemini" => Ok(Self::Gemini),
            other => Err(format!("Unsupported backend '{other}'")),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    SendMessage {
        message: String,
        images: Option<Vec<ImageAttachment>>,
    },
    CancelConversation,
    GetSettings,
    ListSessions,
    ResumeSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    ListProfiles,
    SwitchProfile {
        profile_name: String,
    },
    GetModuleSchemas,
    ListModels,
    UpdateSettings {
        settings: Value,
        persist: bool,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StartupMcpTransport {
    Http {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct StartupMcpServer {
    pub name: String,
    pub transport: StartupMcpTransport,
}

/// Identity of an agent definition, used by backends that support native agent
/// flags (e.g. Claude CLI `--agents`/`--agent`).
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub id: String,
    pub description: String,
    pub instructions: String,
}

#[derive(Clone)]
pub enum BackendCommandHandle {
    Tycode(Arc<Mutex<ChildStdin>>),
    Codex(CodexCommandHandle),
    Claude(ClaudeCommandHandle),
    Kiro(KiroCommandHandle),
    Gemini(GeminiCommandHandle),
    TydeServer(TydeServerProxyCommandHandle),
}

impl BackendCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match self {
            Self::Tycode(stdin) => {
                let payload = tycode_payload_for_command(command);
                if payload.is_empty() {
                    return Ok(());
                }
                let mut guard = stdin.lock().await;
                guard
                    .write_all(payload.as_bytes())
                    .await
                    .map_err(|e| format!("{e:?}"))
            }
            Self::Codex(handle) => handle.execute(command).await,
            Self::Claude(handle) => handle.execute(command).await,
            Self::Kiro(handle) => handle.execute(command).await,
            Self::Gemini(handle) => handle.execute(command).await,
            Self::TydeServer(handle) => handle.execute(command).await,
        }
    }
}

fn tycode_payload_for_command(command: SessionCommand) -> String {
    match command {
        SessionCommand::SendMessage { message, images } => {
            let payload = match images {
                Some(imgs) if !imgs.is_empty() => json!({
                    "UserInputWithImages": {
                        "text": message,
                        "images": imgs
                    }
                }),
                _ => json!({ "UserInput": message }),
            };
            format!("{payload}\n")
        }
        SessionCommand::CancelConversation => "CANCEL\n".to_string(),
        SessionCommand::GetSettings => "\"GetSettings\"\n".to_string(),
        SessionCommand::ListSessions => "\"ListSessions\"\n".to_string(),
        SessionCommand::ResumeSession { session_id } => {
            format!(
                "{}\n",
                json!({ "ResumeSession": { "session_id": session_id } })
            )
        }
        SessionCommand::DeleteSession { session_id } => {
            // Tycode ChatActorMessage does not currently include a DeleteSession
            // variant. Route through its built-in slash command instead.
            format!(
                "{}\n",
                json!({ "UserInput": format!("/sessions delete {session_id}") })
            )
        }
        SessionCommand::ListProfiles => "\"ListProfiles\"\n".to_string(),
        SessionCommand::SwitchProfile { profile_name } => {
            format!(
                "{}\n",
                json!({ "SwitchProfile": { "profile_name": profile_name } })
            )
        }
        SessionCommand::GetModuleSchemas => "\"GetModuleSchemas\"\n".to_string(),
        SessionCommand::ListModels => String::new(), // Tycode does not support model listing
        SessionCommand::UpdateSettings { settings, persist } => {
            format!(
                "{}\n",
                json!({
                    "SaveSettings": {
                        "settings": settings,
                        "persist": persist
                    }
                })
            )
        }
    }
}

fn tycode_mcp_servers_json(
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Option<String>, String> {
    if startup_mcp_servers.is_empty() {
        return Ok(None);
    }

    let mut out = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }

        match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut cfg = serde_json::Map::new();
                cfg.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    cfg.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .map_err(|err| format!("Failed to serialize MCP headers: {err}"))?,
                    );
                }
                out.insert(name.to_string(), Value::Object(cfg));
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                let mut cfg = serde_json::Map::new();
                cfg.insert(
                    "command".to_string(),
                    Value::String(trimmed_command.to_string()),
                );
                if !args.is_empty() {
                    cfg.insert(
                        "args".to_string(),
                        serde_json::to_value(args)
                            .map_err(|err| format!("Failed to serialize MCP args: {err}"))?,
                    );
                }
                if !env.is_empty() {
                    cfg.insert(
                        "env".to_string(),
                        serde_json::to_value(env)
                            .map_err(|err| format!("Failed to serialize MCP env: {err}"))?,
                    );
                }
                out.insert(name.to_string(), Value::Object(cfg));
            }
        }
    }

    if out.is_empty() {
        return Ok(None);
    }

    serde_json::to_string(&Value::Object(out))
        .map(Some)
        .map_err(|err| format!("Failed to serialize startup MCP servers: {err}"))
}

/// A proxy session that forwards commands to a remote Tyde server over the
/// protocol connection. Events come back through the TydeServerConnection's
/// reader task and are re-emitted as Tauri events — no local event receiver.
pub struct TydeServerProxySession {
    pub connection: Arc<TydeServerConnection>,
    pub server_conversation_id: u64,
    pub backend_kind: BackendKind,
}

#[derive(Clone)]
pub struct TydeServerProxyCommandHandle {
    connection: Arc<TydeServerConnection>,
    server_conversation_id: u64,
}

impl TydeServerProxyCommandHandle {
    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        let params = session_command_to_json(self.server_conversation_id, &command);
        let cmd_name = match &command {
            SessionCommand::SendMessage { .. } => "send_message",
            SessionCommand::CancelConversation => "cancel_conversation",
            SessionCommand::GetSettings => "get_settings",
            SessionCommand::ListSessions => "list_sessions",
            SessionCommand::ResumeSession { .. } => "resume_session",
            SessionCommand::DeleteSession { .. } => "delete_session",
            SessionCommand::ListProfiles => "list_profiles",
            SessionCommand::SwitchProfile { .. } => "switch_profile",
            SessionCommand::GetModuleSchemas => "get_module_schemas",
            SessionCommand::ListModels => "list_models",
            SessionCommand::UpdateSettings { .. } => "update_settings",
        };
        self.connection.invoke(cmd_name, params).await?;
        Ok(())
    }
}

fn session_command_to_json(conversation_id: u64, command: &SessionCommand) -> Value {
    match command {
        SessionCommand::SendMessage { message, images } => json!({
            "conversation_id": conversation_id,
            "message": message,
            "images": images,
        }),
        SessionCommand::CancelConversation => json!({ "conversation_id": conversation_id }),
        SessionCommand::GetSettings => json!({ "conversation_id": conversation_id }),
        SessionCommand::ListSessions => json!({ "conversation_id": conversation_id }),
        SessionCommand::ResumeSession { session_id } => json!({
            "conversation_id": conversation_id,
            "session_id": session_id,
        }),
        SessionCommand::DeleteSession { session_id } => json!({
            "conversation_id": conversation_id,
            "session_id": session_id,
        }),
        SessionCommand::ListProfiles => json!({ "conversation_id": conversation_id }),
        SessionCommand::SwitchProfile { profile_name } => json!({
            "conversation_id": conversation_id,
            "profile_name": profile_name,
        }),
        SessionCommand::GetModuleSchemas => json!({ "conversation_id": conversation_id }),
        SessionCommand::ListModels => json!({ "conversation_id": conversation_id }),
        SessionCommand::UpdateSettings { settings, persist } => json!({
            "conversation_id": conversation_id,
            "settings": settings,
            "persist": persist,
        }),
    }
}

pub enum BackendSession {
    Tycode(TycodeSession),
    Codex(CodexSession),
    Claude(ClaudeSession),
    Kiro(KiroSession),
    Gemini(GeminiSession),
    TydeServer(TydeServerProxySession),
}

pub struct TycodeSession {
    bridge: SubprocessBridge,
    steering_root: Option<TycodeSteeringRoot>,
}

impl TycodeSession {
    fn command_handle(&self) -> Arc<Mutex<ChildStdin>> {
        self.bridge.stdin()
    }

    async fn shutdown(self) {
        self.bridge.shutdown().await;
        if let Some(steering_root) = self.steering_root {
            steering_root.cleanup().await;
        }
    }
}

struct TycodeSteeringRoot {
    transport: BackendTransport,
    path: String,
}

impl TycodeSteeringRoot {
    async fn create(transport: &BackendTransport, content: &str) -> Result<Self, String> {
        match transport {
            BackendTransport::Local => {
                let root = crate::steering::write_tycode_steering_root(content)?;
                Ok(Self {
                    transport: transport.clone(),
                    path: root.to_string_lossy().to_string(),
                })
            }
            BackendTransport::Ssh { .. } => {
                let id = std::process::id();
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let root = format!("/tmp/tyde-tycode-steering-{id}-{ts}");
                let dir = format!("{root}/.tycode");
                let file = format!("{dir}/tyde_steering.md");
                let cmd = format!(
                    "mkdir -p {} && printf '%s' {} > {}",
                    shell_quote_arg(&dir),
                    shell_quote_arg(content),
                    shell_quote_arg(&file),
                );
                let output = transport.run_shell_command(&cmd).await?;
                if !output.status.success() {
                    return Err(format!(
                        "Failed to write remote Tycode steering root: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                Ok(Self {
                    transport: transport.clone(),
                    path: root,
                })
            }
        }
    }

    fn workspace_root(&self) -> String {
        match &self.transport {
            BackendTransport::Local => self.path.clone(),
            BackendTransport::Ssh { host } => to_remote_uri(host, &self.path),
        }
    }

    async fn cleanup(self) {
        match self.transport {
            BackendTransport::Local => {
                let path = PathBuf::from(&self.path);
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(
                            "Failed to remove Tycode steering root {}: {e}",
                            path.display()
                        );
                    }
                }
            }
            transport @ BackendTransport::Ssh { .. } => {
                let cmd = format!("rm -rf {}", shell_quote_arg(&self.path));
                match transport.run_shell_command(&cmd).await {
                    Ok(output) if !output.status.success() => {
                        tracing::warn!(
                            "Failed to remove remote Tycode steering root {}: {}",
                            self.path,
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to remove remote Tycode steering root {}: {e}",
                            self.path
                        );
                    }
                    Ok(_) => {}
                }
            }
        }
    }
}

impl BackendSession {
    pub async fn spawn(
        kind: BackendKind,
        launch: &BackendLaunchTarget,
        workspace_roots: &[String],
        ephemeral: bool,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        skill_dir: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        // For non-Claude backends, merge agent instructions into the steering
        // content since they don't support --agents/--agent flags.
        let merged_steering: Option<String>;
        let effective_steering = if kind != BackendKind::Claude {
            if let Some(identity) = agent_identity {
                let mut parts = vec![identity.instructions.clone()];
                if let Some(s) = steering_content {
                    if !s.trim().is_empty() {
                        parts.push(s.to_string());
                    }
                }
                merged_steering = Some(parts.join("\n\n"));
                merged_steering.as_deref()
            } else {
                steering_content
            }
        } else {
            steering_content
        };

        match kind {
            BackendKind::Tycode => {
                // Tycode reads steering from `.tycode/*.md` in workspace roots.
                // Inject Tyde steering into a temp workspace root and append it.
                let mut roots = workspace_roots.to_vec();
                if let Some(dir) = skill_dir {
                    roots.push(dir.to_string());
                }
                let steering_root = match effective_steering
                    .filter(|content| !content.trim().is_empty())
                {
                    Some(content) => {
                        let root = TycodeSteeringRoot::create(&launch.transport, content).await?;
                        roots.push(root.workspace_root());
                        Some(root)
                    }
                    None => None,
                };
                let (bridge, rx) = SubprocessBridge::spawn(
                    &launch.executable_path,
                    &roots,
                    tycode_mcp_servers_json(startup_mcp_servers)?.as_deref(),
                    ephemeral,
                )
                .await?;
                Ok((
                    Self::Tycode(TycodeSession {
                        bridge,
                        steering_root,
                    }),
                    rx,
                ))
            }
            BackendKind::Codex => {
                let (session, rx) = if ephemeral {
                    CodexSession::spawn_ephemeral(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                } else {
                    CodexSession::spawn(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                };
                Ok((Self::Codex(session), rx))
            }
            BackendKind::Claude => {
                let (session, rx) = if ephemeral {
                    ClaudeSession::spawn_ephemeral(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        steering_content,
                        agent_identity,
                        skill_dir,
                    )
                    .await?
                } else {
                    ClaudeSession::spawn(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        steering_content,
                        agent_identity,
                        skill_dir,
                    )
                    .await?
                };
                Ok((Self::Claude(session), rx))
            }
            BackendKind::Kiro => {
                let (session, rx) = if ephemeral {
                    KiroSession::spawn_ephemeral(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                } else {
                    KiroSession::spawn(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                };
                Ok((Self::Kiro(session), rx))
            }
            BackendKind::Gemini => {
                let (session, rx) = if ephemeral {
                    GeminiSession::spawn_ephemeral(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                } else {
                    GeminiSession::spawn(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                    )
                    .await?
                };
                Ok((Self::Gemini(session), rx))
            }
        }
    }

    pub async fn spawn_admin(
        kind: BackendKind,
        launch: &BackendLaunchTarget,
        workspace_roots: &[String],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        match kind {
            BackendKind::Tycode => {
                let (bridge, rx) =
                    SubprocessBridge::spawn(&launch.executable_path, workspace_roots, None, true)
                        .await?;
                Ok((
                    Self::Tycode(TycodeSession {
                        bridge,
                        steering_root: None,
                    }),
                    rx,
                ))
            }
            BackendKind::Codex => {
                let (session, rx) =
                    CodexSession::spawn_admin(workspace_roots, launch.transport.clone(), &[], None)
                        .await?;
                Ok((Self::Codex(session), rx))
            }
            BackendKind::Claude => {
                let (session, rx) = ClaudeSession::spawn(
                    workspace_roots,
                    launch.transport.clone(),
                    &[],
                    None,
                    None,
                    None,
                )
                .await?;
                Ok((Self::Claude(session), rx))
            }
            BackendKind::Kiro => {
                let (session, rx) =
                    KiroSession::spawn_admin(workspace_roots, launch.transport.clone(), &[], None)
                        .await?;
                Ok((Self::Kiro(session), rx))
            }
            BackendKind::Gemini => {
                let (session, rx) = GeminiSession::spawn_admin(
                    workspace_roots,
                    launch.transport.clone(),
                    &[],
                    None,
                )
                .await?;
                Ok((Self::Gemini(session), rx))
            }
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Tycode(_) => BackendKind::Tycode,
            Self::Codex(_) => BackendKind::Codex,
            Self::Claude(_) => BackendKind::Claude,
            Self::Kiro(_) => BackendKind::Kiro,
            Self::Gemini(_) => BackendKind::Gemini,
            Self::TydeServer(proxy) => proxy.backend_kind,
        }
    }

    pub fn command_handle(&self) -> BackendCommandHandle {
        match self {
            Self::Tycode(session) => BackendCommandHandle::Tycode(session.command_handle()),
            Self::Codex(session) => BackendCommandHandle::Codex(session.command_handle()),
            Self::Claude(session) => BackendCommandHandle::Claude(session.command_handle()),
            Self::Kiro(session) => BackendCommandHandle::Kiro(session.command_handle()),
            Self::Gemini(session) => BackendCommandHandle::Gemini(session.command_handle()),
            Self::TydeServer(proxy) => {
                BackendCommandHandle::TydeServer(TydeServerProxyCommandHandle {
                    connection: proxy.connection.clone(),
                    server_conversation_id: proxy.server_conversation_id,
                })
            }
        }
    }

    pub async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        match self {
            Self::Claude(session) => session.set_subagent_emitter(emitter).await,
            Self::Codex(session) => session.set_subagent_emitter(emitter).await,
            Self::Gemini(_) | Self::Tycode(_) | Self::Kiro(_) | Self::TydeServer(_) => {}
        }
    }

    pub async fn shutdown(self) {
        match self {
            Self::Tycode(session) => session.shutdown().await,
            Self::Codex(session) => session.shutdown().await,
            Self::Claude(session) => session.shutdown().await,
            Self::Kiro(session) => session.shutdown().await,
            Self::Gemini(session) => session.shutdown().await,
            Self::TydeServer(proxy) => {
                // Tell the remote server to close this conversation
                let _ = proxy
                    .connection
                    .invoke(
                        "close_conversation",
                        json!({ "conversation_id": proxy.server_conversation_id }),
                    )
                    .await;
            }
        }
    }
}
