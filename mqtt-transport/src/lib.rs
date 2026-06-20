mod chunking;
mod client;
mod config;
mod error;
mod framing;
mod link;
mod link_native;
mod protocol_driver;
mod reconnect;
mod rendezvous;
mod session;
mod stream;
mod topic;
mod types;

pub use client::{connect, connect_ephemeral};
pub use config::{MqttConnectConfig, ParticipantRole};
pub use error::{
    CounterViolation, CryptoError, FramingError, MqttTransportError, PublishRejection,
};
pub use protocol::BrokerUrl;
pub use reconnect::{
    MqttReconnectBackoff, RECONNECT_INITIAL, RECONNECT_MAX, ReconnectBackoffError,
};
pub use stream::EnvelopeStream;
pub use types::{
    BrokerAuth, BrokerEndpoint, DEFAULT_MOBILE_MQTT_BROKER_URL, MOBILE_QR_VERSION,
    MQTT_CLEAN_START, MQTT_QOS_AT_LEAST_ONCE, MQTT_RETAIN, MQTT_TRANSPORT_PROTOCOL_VERSION,
    MQTT_VERSION, MobilePairingQrPayload, MqttTransportPolicy, PreSharedKey, RoomId,
    TransportTypeError, default_mobile_broker_endpoint, validate_broker_url,
};

pub use topic::{
    ParsedTopic, TopicDirection, client_to_host_topic, host_to_client_topic, parse_topic,
    topic_for_direction,
};
