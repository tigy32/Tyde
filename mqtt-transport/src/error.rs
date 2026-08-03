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

    #[error("managed MQTT session reached its credential renewal deadline")]
    ManagedSessionExpired,

    #[error(
        "MQTT PUBACK did not match an outstanding publish (packet id {packet_id:?}, token {token:?})"
    )]
    PublishAckMismatch {
        packet_id: Option<u16>,
        token: Option<u64>,
    },

    #[error("timed out waiting for MQTT PUBACK for token {token} after {timeout_ms}ms")]
    PublishAckTimeout { token: u64, timeout_ms: u64 },

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
        match self {
            // A NotAuthorized PUBACK means the broker's authorizer no longer
            // accepts this connection's grant (AWS re-validates the CONNECT
            // token roughly every 5 minutes). Reconnecting with freshly minted
            // credentials recovers; callers must pair this with
            // `invalidates_managed_credentials` so the retry does not reuse the
            // rejected grant.
            Self::PublishRejected { .. }
            | Self::BrokerConnect { .. }
            | Self::Subscribe { .. }
            | Self::SubscribeRejected { .. }
            | Self::Publish { .. }
            | Self::PublishAckTimeout { .. }
            | Self::BrokerDisconnected { .. }
            | Self::ManagedSessionExpired
            | Self::ActorClosed => true,
            _ => false,
        }
    }

    /// True when the failure indicates the managed broker grant itself is no
    /// longer accepted, so a retry must mint fresh credentials instead of
    /// reusing a cached grant.
    pub fn invalidates_managed_credentials(&self) -> bool {
        match self {
            Self::PublishRejected { reason } => reason.is_not_authorized(),
            Self::ManagedSessionExpired => true,
            _ => false,
        }
    }
}

/// Failure delivered on an outbound write acknowledgement. Write acks cross the
/// `io::Error` boundary in `EnvelopeStream`, which erases concrete types; this
/// carries the retryability and credential classification of the underlying
/// [`MqttTransportError`] so reconnect policy does not depend on which side of
/// the stream observed the failure first.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct WriteAckError {
    message: String,
    retryable: bool,
    invalidates_managed_credentials: bool,
}

impl WriteAckError {
    pub(crate) fn from_error(error: &MqttTransportError) -> Self {
        Self {
            message: error.to_string(),
            retryable: error.is_retryable(),
            invalidates_managed_credentials: error.invalidates_managed_credentials(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn invalidates_managed_credentials(&self) -> bool {
        self.invalidates_managed_credentials
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

    pub fn is_not_authorized(&self) -> bool {
        self.code == 0x87
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

#[cfg(test)]
mod tests {
    use super::*;

    // NotAuthorized used to be classified terminal, which permanently stopped
    // the mobile reconnect loop when AWS IoT's periodic authorizer refresh
    // rejected an aging grant (confirmed in production: PUBACK 135 ~10 minutes
    // into every managed session). It is retryable *with fresh credentials*;
    // `invalidates_managed_credentials` carries the "do not reuse the cached
    // grant" half of that contract.
    #[test]
    fn authorization_rejection_retries_with_fresh_credentials() {
        let not_authorized = MqttTransportError::PublishRejected {
            reason: PublishRejection {
                code: 0x87,
                code_name: "NotAuthorized".to_owned(),
                reason_string: None,
            },
        };
        let quota = MqttTransportError::PublishRejected {
            reason: PublishRejection {
                code: PUBACK_QUOTA_EXCEEDED,
                code_name: "QuotaExceeded".to_owned(),
                reason_string: None,
            },
        };

        assert!(not_authorized.is_retryable());
        assert!(not_authorized.invalidates_managed_credentials());
        assert!(quota.is_retryable());
        assert!(!quota.invalidates_managed_credentials());
        assert!(MqttTransportError::ManagedSessionExpired.invalidates_managed_credentials());

        let write_ack = WriteAckError::from_error(&not_authorized);
        assert!(write_ack.is_retryable());
        assert!(write_ack.invalidates_managed_credentials());
    }
}
