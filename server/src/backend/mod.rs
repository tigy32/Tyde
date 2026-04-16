pub mod acp;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod kiro;
pub mod mock;
pub mod subprocess;
pub mod tycode;

use std::collections::HashMap;

use protocol::{
    AgentInput, BackendKind, ChatEvent, ImageData, SendMessagePayload, SessionId, SpawnCostHint,
};
use serde_json::Value;
use tokio::sync::mpsc;

use self::subprocess::ImageAttachment;

pub(crate) fn protocol_images_to_attachments(
    images: Option<Vec<ImageData>>,
) -> Option<Vec<ImageAttachment>> {
    let attachments = images
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, image)| ImageAttachment {
            data: image.data,
            media_type: image.media_type,
            name: format!("image-{}", index + 1),
            size: 0,
        })
        .collect::<Vec<_>>();

    if attachments.is_empty() {
        None
    } else {
        Some(attachments)
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

#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub id: String,
    pub description: String,
    pub instructions: String,
}

#[derive(Debug, Clone)]
pub struct BackendSession {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub title: Option<String>,
    pub token_count: Option<u64>,
    pub created_at_ms: Option<u64>,
    pub updated_at_ms: Option<u64>,
    pub resumable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BackendSpawnConfig {
    pub cost_hint: Option<SpawnCostHint>,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
}

/// Output stream of ChatEvents from a backend session.
/// The agent actor reads from this while independently sending AgentInput
/// through the Backend handle — true duplex.
pub struct EventStream {
    rx: mpsc::Receiver<ChatEvent>,
}

impl EventStream {
    pub fn new(rx: mpsc::Receiver<ChatEvent>) -> Self {
        Self { rx }
    }

    /// Receive the next ChatEvent from the backend.
    /// Returns None when the backend has terminated.
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        self.rx.recv().await
    }
}

/// A coding agent backend session handle.
///
/// Created via `Backend::spawn()` which returns `(Self, EventStream)`.
/// The handle is used to send input; the EventStream is used to read output.
/// Backends are not object-safe — the agent actor knows the concrete type.
pub trait Backend: Send + 'static {
    /// Create a new backend session.
    /// Returns a handle to send input and an EventStream to read output.
    /// The backend must start the session with `initial_input` and know its
    /// native resumable session ID before returning.
    fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: SendMessagePayload,
    ) -> impl std::future::Future<Output = Result<(Self, EventStream), String>> + Send
    where
        Self: Sized;

    /// Resume an existing backend session.
    fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> impl std::future::Future<Output = Result<(Self, EventStream), String>> + Send
    where
        Self: Sized;

    /// Enumerate resumable sessions known to this backend.
    fn list_sessions()
    -> impl std::future::Future<Output = Result<Vec<BackendSession>, String>> + Send
    where
        Self: Sized;

    /// Return the backend-native session ID for this live handle.
    fn session_id(&self) -> SessionId;

    /// Send an input event to the backend.
    /// Returns false if the backend has terminated and can't accept input.
    fn send(&self, input: AgentInput) -> impl std::future::Future<Output = bool> + Send;

    /// Interrupt the currently active turn, if any.
    /// Returns false if the backend has terminated or doesn't support interruption.
    fn interrupt(&self) -> impl std::future::Future<Output = bool> + Send;
}
