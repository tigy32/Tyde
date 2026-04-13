use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use tyde_protocol::protocol::SessionSettingsData;

use crate::backends::tycode::ImageAttachment;

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
        settings: SessionSettingsData,
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
