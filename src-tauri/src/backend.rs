use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, Mutex};

use crate::claude::{ClaudeCommandHandle, ClaudeSession, SubAgentEmitter};
use crate::codex::{CodexCommandHandle, CodexSession};
use crate::kiro::{KiroCommandHandle, KiroSession};
use crate::subprocess::{ImageAttachment, SubprocessBridge};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Tycode,
    Codex,
    Claude,
    Kiro,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tycode => "tycode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kiro => "kiro",
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

#[derive(Clone)]
pub enum BackendCommandHandle {
    Tycode(Arc<Mutex<ChildStdin>>),
    Codex(CodexCommandHandle),
    Claude(ClaudeCommandHandle),
    Kiro(KiroCommandHandle),
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

pub enum BackendSession {
    Tycode(SubprocessBridge),
    Codex(CodexSession),
    Claude(ClaudeSession),
    Kiro(KiroSession),
}

impl BackendSession {
    pub async fn spawn(
        kind: BackendKind,
        executable_path: &str,
        workspace_roots: &[String],
        ephemeral: bool,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        match kind {
            BackendKind::Tycode => {
                let (bridge, rx) = SubprocessBridge::spawn(
                    executable_path,
                    workspace_roots,
                    tycode_mcp_servers_json(startup_mcp_servers)?.as_deref(),
                    ephemeral,
                ).await?;
                Ok((Self::Tycode(bridge), rx))
            }
            BackendKind::Codex => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) = if ephemeral {
                    CodexSession::spawn_ephemeral(workspace_roots, ssh_host, startup_mcp_servers, steering_content)
                        .await?
                } else {
                    CodexSession::spawn(workspace_roots, ssh_host, startup_mcp_servers, steering_content).await?
                };
                Ok((Self::Codex(session), rx))
            }
            BackendKind::Claude => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) = if ephemeral {
                    ClaudeSession::spawn_ephemeral(workspace_roots, ssh_host, startup_mcp_servers, steering_content)
                        .await?
                } else {
                    ClaudeSession::spawn(workspace_roots, ssh_host, startup_mcp_servers, steering_content).await?
                };
                Ok((Self::Claude(session), rx))
            }
            BackendKind::Kiro => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) = if ephemeral {
                    KiroSession::spawn_ephemeral(workspace_roots, ssh_host, startup_mcp_servers, steering_content)
                        .await?
                } else {
                    KiroSession::spawn(workspace_roots, ssh_host, startup_mcp_servers, steering_content).await?
                };
                Ok((Self::Kiro(session), rx))
            }
        }
    }

    pub async fn spawn_admin(
        kind: BackendKind,
        executable_path: &str,
        workspace_roots: &[String],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        match kind {
            BackendKind::Tycode => {
                let (bridge, rx) =
                    SubprocessBridge::spawn(executable_path, workspace_roots, None, true).await?;
                Ok((Self::Tycode(bridge), rx))
            }
            BackendKind::Codex => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) =
                    CodexSession::spawn_admin(workspace_roots, ssh_host, &[], None).await?;
                Ok((Self::Codex(session), rx))
            }
            BackendKind::Claude => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) = ClaudeSession::spawn(workspace_roots, ssh_host, &[], None).await?;
                Ok((Self::Claude(session), rx))
            }
            BackendKind::Kiro => {
                let ssh_host = if executable_path.is_empty() {
                    None
                } else {
                    Some(executable_path.to_string())
                };
                let (session, rx) =
                    KiroSession::spawn_admin(workspace_roots, ssh_host, &[], None).await?;
                Ok((Self::Kiro(session), rx))
            }
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Tycode(_) => BackendKind::Tycode,
            Self::Codex(_) => BackendKind::Codex,
            Self::Claude(_) => BackendKind::Claude,
            Self::Kiro(_) => BackendKind::Kiro,
        }
    }

    pub fn command_handle(&self) -> BackendCommandHandle {
        match self {
            Self::Tycode(bridge) => BackendCommandHandle::Tycode(bridge.stdin()),
            Self::Codex(session) => BackendCommandHandle::Codex(session.command_handle()),
            Self::Claude(session) => BackendCommandHandle::Claude(session.command_handle()),
            Self::Kiro(session) => BackendCommandHandle::Kiro(session.command_handle()),
        }
    }

    pub async fn session_id(&self) -> Option<String> {
        match self {
            Self::Tycode(_) => None,
            Self::Codex(session) => session.session_id().await,
            Self::Claude(session) => session.session_id().await,
            Self::Kiro(session) => session.session_id().await,
        }
    }

    pub async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        match self {
            Self::Claude(session) => session.set_subagent_emitter(emitter).await,
            Self::Codex(session) => session.set_subagent_emitter(emitter).await,
            _ => {}
        }
    }

    pub async fn shutdown(self) {
        match self {
            Self::Tycode(bridge) => bridge.shutdown().await,
            Self::Codex(session) => session.shutdown().await,
            Self::Claude(session) => session.shutdown().await,
            Self::Kiro(session) => session.shutdown().await,
        }
    }
}
