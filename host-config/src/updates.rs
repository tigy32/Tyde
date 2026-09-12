use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    Release,
    Preview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePreferences {
    pub channel: UpdateChannel,
    pub automatic: bool,
    pub remind_after: u64,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self {
            channel: UpdateChannel::Release,
            automatic: true,
            remind_after: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Checking,
    Available,
    Downloading,
    Installing,
    Error,
}

impl UpdatePhase {
    pub fn busy(self) -> bool {
        matches!(self, Self::Checking | Self::Downloading | Self::Installing)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUpdateStatus {
    pub revision: u64,
    pub current_version: String,
    pub preferences: UpdatePreferences,
    pub phase: UpdatePhase,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub prompt: bool,
    pub last_checked: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateDismissal {
    Never,
    NotNow,
}
