use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_runtime::AgentInfo;

// ---------------------------------------------------------------------------
// Wire format: newline-delimited JSON.
//
// This is the Tyde protocol — the same commands and events that the frontend
// uses via Tauri invoke/listen, wrapped in a transport envelope for
// request-response correlation and state sync.
//
// Command names and param shapes match the existing Tauri commands exactly.
// Event names and payload shapes match existing Tauri events exactly.
// ---------------------------------------------------------------------------

pub const PROTOCOL_VERSION: u32 = 1;

fn deserialize_u64_from_number_or_string<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct U64Visitor;

    impl<'de> serde::de::Visitor<'de> for U64Visitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("u64 as number or string")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v < 0 {
                return Err(E::custom("negative values are not valid u64"));
            }
            Ok(v as u64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            v.parse::<u64>()
                .map_err(|_| E::custom(format!("invalid u64 string: {v}")))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(U64Visitor)
}

// ---------------------------------------------------------------------------
// Client → Server: invoke a Tyde command
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientFrame {
    /// Invoke a Tyde command. `command` is the Tauri command name (e.g.
    /// "create_conversation", "send_message", "spawn_agent"). `params` is
    /// the same JSON object the frontend would pass to `invoke()`.
    Invoke {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        command: String,
        params: Value,
    },

    /// Connection handshake — the only message type that doesn't map to a
    /// Tauri command. Sent once on connect to sync state.
    Handshake {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        protocol_version: u32,
        last_agent_event_seq: u64,
        last_chat_event_seqs: HashMap<String, u64>,
    },
}

// ---------------------------------------------------------------------------
// Server → Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    /// Successful response to an Invoke or Handshake.
    Result {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        data: Value,
    },

    /// Error response to an Invoke or Handshake.
    Error {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        error: String,
    },

    /// Pushed Tauri event. `event` is the Tauri event name (e.g.
    /// "chat-event", "agent-changed"). `payload` is the same shape
    /// the frontend receives via `listen()`.
    Event {
        event: String,
        seq: Option<u64>,
        payload: Value,
    },

    /// Server is shutting down — clean disconnect.
    Shutdown { reason: String },
}

// ---------------------------------------------------------------------------
// Handshake response data (embedded in Result.data for handshake)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResult {
    pub protocol_version: u32,
    pub agents: Vec<AgentInfo>,
    pub conversations: Vec<ConversationSnapshot>,
    #[serde(default)]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub conversation_id: u64,
    pub backend_kind: String,
    pub workspace_roots: Vec<String>,
    pub chat_event_seq: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::ClientFrame;

    #[test]
    fn handshake_roundtrips_chat_cursor_map() {
        let mut cursors = HashMap::new();
        cursors.insert("42".to_string(), 7);
        let frame = ClientFrame::Handshake {
            req_id: 0,
            protocol_version: super::PROTOCOL_VERSION,
            last_agent_event_seq: 3,
            last_chat_event_seqs: cursors.clone(),
        };
        let json = serde_json::to_string(&frame).expect("serialize handshake");
        let parsed: ClientFrame = serde_json::from_str(&json).expect("deserialize handshake");

        match parsed {
            ClientFrame::Handshake {
                req_id,
                protocol_version,
                last_agent_event_seq,
                last_chat_event_seqs,
            } => {
                assert_eq!(req_id, 0);
                assert_eq!(protocol_version, super::PROTOCOL_VERSION);
                assert_eq!(last_agent_event_seq, 3);
                assert_eq!(last_chat_event_seqs, cursors);
            }
            _ => panic!("expected handshake frame"),
        }
    }

    #[test]
    fn invoke_req_id_accepts_string_or_number() {
        let as_string = r#"{"type":"Invoke","req_id":"1","command":"list_agents","params":{}}"#;
        let parsed: ClientFrame = serde_json::from_str(as_string).expect("parse string req_id");
        match parsed {
            ClientFrame::Invoke { req_id, .. } => assert_eq!(req_id, 1),
            _ => panic!("expected invoke frame"),
        }

        let as_number = r#"{"type":"Invoke","req_id":2,"command":"list_agents","params":{}}"#;
        let parsed: ClientFrame = serde_json::from_str(as_number).expect("parse numeric req_id");
        match parsed {
            ClientFrame::Invoke { req_id, .. } => assert_eq!(req_id, 2),
            _ => panic!("expected invoke frame"),
        }
    }
}
