use std::fmt;

use thiserror::Error;

/// MQTT5 PUBACK reason code for "Quota exceeded" (0x97). Used to classify
/// broker-side rate limiting for publish pacing, without naming any MQTT
/// library's reason-code enum at the seam.
pub(crate) const PUBACK_QUOTA_EXCEEDED: u8 = 0x97;

/// Transport-neutral source error carried by the seam. The native backend boxes
/// rumqttc's `ConnectionError`/`ClientError`; a wasm backend boxes its own error
/// type. Either way the driver only ever needs `Display`.
type BackendError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum MqttTransportError {
    #[error("MQTT broker connection failed: {source}")]
    BrokerConnect { source: BackendError },

    #[error("MQTT subscribe request failed: {source}")]
    Subscribe { source: BackendError },

    #[error("MQTT subscribe was rejected: {reason}")]
    SubscribeRejected { reason: String },

    #[error("MQTT publish request failed: {source}")]
    Publish { source: BackendError },

    #[error("MQTT publish was rejected: {reason}")]
    PublishRejected { reason: PublishRejection },

    #[error(
        "MQTT PUBACK did not match an outstanding publish (packet id {packet_id:?}, token {token:?})"
    )]
    PublishAckMismatch {
        packet_id: Option<u16>,
        token: Option<u64>,
    },

    #[error("transport framing error: {0}")]
    Framing(#[from] FramingError),

    #[error("transport crypto error: {0}")]
    Crypto(#[from] CryptoError),

    #[error("MQTT broker disconnected: {reason}")]
    BrokerDisconnected { reason: String },

    #[error("invalid MQTT transport configuration: {message}")]
    Configuration { message: String },

    #[error("MQTT retained message rejected on topic {topic}")]
    RetainedMessage { topic: String },

    #[error(
        "timed out waiting for MQTT receiver credit for data counter {data_counter} after {timeout_ms}ms"
    )]
    ReceiverCreditTimeout { data_counter: u64, timeout_ms: u64 },

    #[error("MQTT actor stopped before completing the requested operation")]
    ActorClosed,
}

impl MqttTransportError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::BrokerConnect { .. }
                | Self::Subscribe { .. }
                | Self::SubscribeRejected { .. }
                | Self::Publish { .. }
                | Self::PublishRejected { .. }
                | Self::BrokerDisconnected { .. }
                | Self::ActorClosed
        )
    }
}

/// A rejected PUBLISH, in transport-neutral form. `code` is the MQTT5 numeric
/// PUBACK reason code and `code_name` is its human name (e.g. `"QuotaExceeded"`)
/// — the backend fills both from its own reason-code enum so the seam carries no
/// library types. `code_name` preserves the exact text the previous
/// `{PubAckReason:?}` Display produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRejection {
    pub code: u8,
    pub code_name: String,
    pub reason_string: Option<String>,
}

impl PublishRejection {
    pub fn is_quota_exceeded(&self) -> bool {
        self.code == PUBACK_QUOTA_EXCEEDED
    }
}

impl fmt::Display for PublishRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason_string.as_deref() {
            Some(reason) => write!(f, "{}: {reason}", self.code_name),
            None => write!(f, "{}", self.code_name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FramingError {
    #[error("frame is empty")]
    EmptyFrame,

    #[error(
        "unsupported MQTT transport frame version {actual:#04x}, expected {expected:#04x}; update both Tyde clients and re-pair"
    )]
    VersionMismatch { expected: u8, actual: u8 },

    #[error("unknown frame tag {tag:#04x}")]
    UnknownTag { tag: u8 },

    #[error("handshake frame length {actual} is invalid; expected {expected}")]
    InvalidHandshakeLength { expected: usize, actual: usize },

    #[error("data frame is too short: length {actual}, minimum {minimum}")]
    DataFrameTooShort { minimum: usize, actual: usize },

    #[error("invalid UTF-8 topic bytes: {message}")]
    InvalidTopicUtf8 { message: String },

    #[error("invalid MQTT topic: {message}")]
    InvalidTopic { message: String },

    #[error("data frame received before the session key was established")]
    DataBeforeHandshake,

    #[error("handshake frame received after the session key was established")]
    HandshakeAfterSession,

    #[error("rendezvous frame payload length {actual} is invalid; expected {expected}")]
    InvalidRendezvousLength { expected: usize, actual: usize },

    #[error("rendezvous frame authentication failed: {0}")]
    Crypto(#[from] CryptoError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CryptoError {
    #[error("HKDF session key derivation failed")]
    HkdfExpand,

    #[error("AEAD authentication failed")]
    AeadFailure,

    #[error("counter validation failed: {0}")]
    CounterViolation(CounterViolation),

    #[error("send counter rollover would occur")]
    CounterRollover,

    #[error("salt exchange violation: {message}")]
    SaltExchangeViolation { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CounterViolation {
    FirstFrameMustBeZero { actual: u64 },
    ReplayedOlderFrame { last_seen: u64, actual: u64 },
    Gap { last_seen: Option<u64>, actual: u64 },
    CreditBeyondSent { sent_next: u64, credit_next: u64 },
}

impl fmt::Display for CounterViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstFrameMustBeZero { actual } => {
                write!(f, "first data counter must be 0, got {actual}")
            }
            Self::ReplayedOlderFrame { last_seen, actual } => {
                write!(
                    f,
                    "counter {actual} is older than last accepted counter {last_seen}"
                )
            }
            Self::Gap { last_seen, actual } => match last_seen {
                Some(last_seen) => write!(
                    f,
                    "counter gap: last accepted counter {last_seen}, got {actual}"
                ),
                None => write!(f, "counter gap before first frame: got {actual}"),
            },
            Self::CreditBeyondSent {
                sent_next,
                credit_next,
            } => write!(
                f,
                "receiver credit {credit_next} exceeds next local data counter {sent_next}"
            ),
        }
    }
}
