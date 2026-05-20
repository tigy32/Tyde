use serde::{Deserialize, Serialize};

use crate::framing::{DIRECTION_CLIENT_TO_HOST, DIRECTION_HOST_TO_CLIENT};
use crate::topic::{client_to_host_topic, host_to_client_topic};
use crate::types::{BrokerEndpoint, PreSharedKey, RoomId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqttConnectConfig {
    pub endpoint: BrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub role: ParticipantRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    Host,
    Client,
}

impl ParticipantRole {
    pub(crate) const fn client_id_prefix(self) -> &'static str {
        match self {
            Self::Host => "tyde-host",
            Self::Client => "tyde-mobile",
        }
    }

    pub(crate) const fn outbound_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_HOST_TO_CLIENT,
            Self::Client => DIRECTION_CLIENT_TO_HOST,
        }
    }

    pub(crate) const fn inbound_direction(self) -> u8 {
        match self {
            Self::Host => DIRECTION_CLIENT_TO_HOST,
            Self::Client => DIRECTION_HOST_TO_CLIENT,
        }
    }

    pub(crate) fn inbound_topic(self, room: &RoomId) -> String {
        match self {
            Self::Host => client_to_host_topic(room),
            Self::Client => host_to_client_topic(room),
        }
    }

    pub(crate) fn outbound_topic(self, room: &RoomId) -> String {
        match self {
            Self::Host => host_to_client_topic(room),
            Self::Client => client_to_host_topic(room),
        }
    }
}
