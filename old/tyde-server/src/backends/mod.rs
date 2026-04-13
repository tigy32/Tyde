pub mod claude;
pub mod codex;
pub mod gemini;
pub mod kiro;
pub mod transport;
pub mod tycode;
pub mod types;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tyde_protocol::protocol::ChatEvent;

use crate::agent::AgentHandle;

use self::claude::{ClaudeSession, SubAgentEmitter};
use self::codex::CodexSession;
use self::gemini::GeminiSession;
use self::kiro::KiroSession;
use self::transport::BackendLaunchTarget;
use self::tycode::TycodeSession;
use self::types::{AgentIdentity, BackendKind, StartupMcpServer};

/// The backend behind an agent. Implemented by real subprocess sessions
/// and by MockSession in tests. Object-safe — stored as Box<dyn Backend>.
pub trait Backend: Send + 'static {
    fn agent_handle(&self) -> AgentHandle;
    fn kind_str(&self) -> String;
    fn tracks_local_session_store(&self) -> bool;
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

// ── Local backend dispatch ──────────────────────────────────────────

pub struct LocalBackendSpawnRequest<'a> {
    pub kind: BackendKind,
    pub launch: &'a BackendLaunchTarget,
    pub workspace_roots: &'a [String],
    pub ephemeral: bool,
    pub startup_mcp_servers: &'a [StartupMcpServer],
    pub steering_content: Option<&'a str>,
    pub agent_identity: Option<&'a AgentIdentity>,
    pub skill_dir: Option<&'a str>,
    pub subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
}

pub enum LocalBackendSession {
    Tycode(TycodeSession),
    Codex(CodexSession),
    Claude(ClaudeSession),
    Kiro(KiroSession),
    Gemini(GeminiSession),
}

impl LocalBackendSession {
    pub async fn spawn(
        request: LocalBackendSpawnRequest<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        let LocalBackendSpawnRequest {
            kind,
            launch,
            workspace_roots,
            ephemeral,
            startup_mcp_servers,
            steering_content,
            agent_identity,
            skill_dir,
            subagent_emitter,
        } = request;
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
                let mut roots = workspace_roots.to_vec();
                if let Some(dir) = skill_dir {
                    roots.push(dir.to_string());
                }
                let (session, rx) = TycodeSession::spawn(
                    &launch.executable_path,
                    &roots,
                    startup_mcp_servers,
                    ephemeral,
                    effective_steering,
                    &launch.transport,
                )
                .await?;
                Ok((Self::Tycode(session), rx))
            }
            BackendKind::Codex => {
                let (session, rx) = if ephemeral {
                    CodexSession::spawn_ephemeral(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                        subagent_emitter.clone(),
                    )
                    .await?
                } else {
                    CodexSession::spawn(
                        workspace_roots,
                        launch.transport.clone(),
                        startup_mcp_servers,
                        effective_steering,
                        subagent_emitter.clone(),
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
                        subagent_emitter.clone(),
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
                        subagent_emitter.clone(),
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
}

impl Backend for LocalBackendSession {
    fn agent_handle(&self) -> AgentHandle {
        match self {
            Self::Tycode(s) => AgentHandle::new(Arc::new(s.command_handle())),
            Self::Codex(s) => AgentHandle::new(Arc::new(s.command_handle())),
            Self::Claude(s) => AgentHandle::new(Arc::new(s.command_handle())),
            Self::Kiro(s) => AgentHandle::new(Arc::new(s.command_handle())),
            Self::Gemini(s) => AgentHandle::new(Arc::new(s.command_handle())),
        }
    }

    fn kind_str(&self) -> String {
        match self {
            Self::Tycode(_) => "tycode",
            Self::Codex(_) => "codex",
            Self::Claude(_) => "claude",
            Self::Kiro(_) => "kiro",
            Self::Gemini(_) => "gemini",
        }
        .to_string()
    }

    fn tracks_local_session_store(&self) -> bool {
        true
    }

    fn shutdown(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            match *self {
                LocalBackendSession::Tycode(s) => s.shutdown().await,
                LocalBackendSession::Codex(s) => s.shutdown().await,
                LocalBackendSession::Claude(s) => s.shutdown().await,
                LocalBackendSession::Kiro(s) => s.shutdown().await,
                LocalBackendSession::Gemini(s) => s.shutdown().await,
            }
        })
    }
}
