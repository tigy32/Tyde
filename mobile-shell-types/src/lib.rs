use std::fmt;

use protocol::BrokerUrl;
pub use protocol::MobileAccessErrorCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalHostId(pub String);

impl LocalHostId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocalHostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeychainSecretId(pub String);

impl fmt::Display for KeychainSecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrokerEndpointSummary {
    pub url: BrokerUrl,
    pub auth: BrokerAuthSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrokerAuthSummary {
    Anonymous,
    UsernamePassword {
        username: String,
        has_password: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomIdSummary(pub String);

impl fmt::Display for RoomIdSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobilePairingPreview {
    pub host_label: String,
    pub broker_url: BrokerUrl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHostSummary {
    pub local_host_id: LocalHostId,
    pub host_label: String,
    pub broker: BrokerEndpointSummary,
    pub room: RoomIdSummary,
    pub credential_fingerprint: String,
    pub auto_connect: bool,
    pub last_connected_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHostsChangedEvent {
    pub hosts: Vec<PairedHostSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHostConnectionStatusEvent {
    pub local_host_id: LocalHostId,
    pub status: PairedHostConnectionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairedHostConnectionStatus {
    Connecting,
    Connected,
    Disconnected {
        reason: String,
    },
    Failed {
        code: MobileAccessErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileShellError {
    pub code: MobileAccessErrorCode,
    pub message: String,
}

pub type MobileShellErrorEvent = MobileShellError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_host_id_round_trips_as_transparent_string() -> Result<(), Box<dyn std::error::Error>> {
        let id = LocalHostId("host-1".to_owned());
        let encoded = serde_json::to_string(&id)?;
        assert_eq!(encoded, "\"host-1\"");
        let decoded: LocalHostId = serde_json::from_str(&encoded)?;
        assert_eq!(decoded, id);
        Ok(())
    }

    #[test]
    fn failed_status_uses_protocol_error_code_shape() -> Result<(), Box<dyn std::error::Error>> {
        let status = PairedHostConnectionStatus::Failed {
            code: MobileAccessErrorCode::TransportFailed,
            message: "network dropped".to_owned(),
        };
        let encoded = serde_json::to_string(&status)?;
        assert!(encoded.contains("transport_failed"));
        let decoded: PairedHostConnectionStatus = serde_json::from_str(&encoded)?;
        assert_eq!(decoded, status);
        Ok(())
    }

    #[test]
    fn broker_auth_summary_does_not_serialize_password() -> Result<(), Box<dyn std::error::Error>> {
        let summary = BrokerAuthSummary::UsernamePassword {
            username: "mobile".to_owned(),
            has_password: true,
        };
        let encoded = serde_json::to_string(&summary)?;
        assert!(encoded.contains("mobile"));
        assert!(encoded.contains("has_password"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("password\":\""));
        Ok(())
    }
}
