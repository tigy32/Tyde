mod chunking;
mod config;
mod error;
mod framing;
mod link;
mod protocol_driver;
mod reconnect;
mod rendezvous;
mod session;
mod stream;
mod time;
mod topic;
mod types;

// MQTT I/O backend + its connect entry point are target-specific. Native uses
// rumqttc (`link_native` + `client`); wasm uses a `web-sys::WebSocket` backend
// (`link_wasm` + `client_wasm`). Both expose the same `connect`/`connect_ephemeral`.
#[cfg(not(target_arch = "wasm32"))]
mod client;
#[cfg(target_arch = "wasm32")]
mod client_wasm;
#[cfg(not(target_arch = "wasm32"))]
mod link_native;
#[cfg(target_arch = "wasm32")]
mod link_wasm;
// Pure codec/ack helpers for the wasm backend; compiled into the wasm build and
// into native test builds (where mqttbytes is a dev-dependency) so they can be
// unit-tested natively.
#[cfg(any(target_arch = "wasm32", test))]
mod wasm_codec;

#[cfg(not(target_arch = "wasm32"))]
pub use client::{connect, connect_ephemeral, connect_managed, connect_managed_ephemeral};
#[cfg(target_arch = "wasm32")]
pub use client_wasm::{connect, connect_ephemeral, connect_managed, connect_managed_ephemeral};
pub use config::{ManagedMqttConnectConfig, MqttConnectConfig, ParticipantRole};
pub use error::{
    CounterViolation, CryptoError, FramingError, MqttTransportError, PublishRejection,
};
pub use protocol::BrokerUrl;
pub use protocol::{
    ManagedBrokerAuthorizerName, ManagedBrokerClientId, ManagedBrokerConnectAuth,
    ManagedBrokerCredentialScope, ManagedBrokerCredentials, ManagedBrokerEndpoint,
    ManagedBrokerGrantId, ManagedBrokerProvider, ManagedBrokerRegion, ManagedBrokerRole,
    ManagedBrokerTopicNamespace, MobilePairingOfferId,
};
pub use reconnect::{
    MqttReconnectBackoff, RECONNECT_INITIAL, RECONNECT_MAX, ReconnectBackoffError,
};
pub use stream::EnvelopeStream;
pub use types::{
    BrokerAuth, BrokerEndpoint, DEFAULT_MOBILE_MQTT_BROKER_URL, LEGACY_MOBILE_QR_VERSION,
    MOBILE_MANAGED_QR_VERSION, MOBILE_QR_VERSION, MQTT_CLEAN_START, MQTT_QOS_AT_LEAST_ONCE,
    MQTT_RETAIN, MQTT_TRANSPORT_PROTOCOL_VERSION, MQTT_VERSION, ManagedMobilePairingQrPayload,
    ManagedMobilePairingQrPayloadParams, MobilePairingQrOffer, MobilePairingQrPayload,
    MqttTransportPolicy, PreSharedKey, RoomId, TransportTypeError, default_mobile_broker_endpoint,
    is_legacy_public_broker_endpoint, parse_mobile_pairing_qr_offer, validate_broker_url,
};

pub use topic::{
    ParsedTopic, TopicDirection, client_to_host_topic, host_to_client_topic,
    managed_client_to_host_topic, managed_host_to_client_topic, managed_topic_for_direction,
    parse_topic, topic_for_direction,
};
