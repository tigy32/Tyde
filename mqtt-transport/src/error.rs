use std::fmt;

use rumqttc::v5::mqttbytes::v5::PubAckReason;
use rumqttc::v5::{ClientError, ConnectionError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MqttTransportError {
    #[error("MQTT broker connection failed: {source}")]
    BrokerConnect { source: Box<ConnectionError> },

    #[error("MQTT subscribe request failed: {source}")]
    Subscribe { source: Box<ClientError> },

    #[error("MQTT subscribe was rejected: {reason}")]
    SubscribeRejected { reason: String },

    #[error("MQTT publish request failed: {source}")]
    Publish { source: Box<ClientError> },

    #[error("MQTT publish was rejected: {reason}")]
    PublishRejected { reason: PublishRejection },

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRejection {
    pub code: PubAckReason,
    pub reason_string: Option<String>,
}

impl PublishRejection {
    pub fn is_quota_exceeded(&self) -> bool {
        self.code == PubAckReason::QuotaExceeded
    }
}

impl fmt::Display for PublishRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason_string.as_deref() {
            Some(reason) => write!(f, "{:?}: {reason}", self.code),
            None => write!(f, "{:?}", self.code),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FramingError {
    #[error("frame is empty")]
    EmptyFrame,

    #[error("unsupported transport version {actual:#04x}, expected {expected:#04x}")]
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
        }
    }
}
