//! Pure MQTT5 codec + acknowledgement-decision helpers for the wasm backend.
//!
//! These are split out of [`link_wasm`](crate::link_wasm) (which is wasm-only and
//! cannot host native tests) so the acknowledgement logic the reviewers flagged
//! can be unit-tested on the native target. The module is compiled into the wasm
//! build (where `mqttbytes` is a normal dependency) and into native **test**
//! builds (where it is a dev-dependency); it is absent from native non-test
//! builds, so it never pulls `mqttbytes` into the shipped native crate.
//!
//! Everything here is pure: it takes the relevant state (e.g. the outstanding
//! publish pkid) as an argument and returns bytes/decisions, with no I/O.

use bytes::BytesMut;
use mqttbytes::QoS;
use mqttbytes::v5::{
    Disconnect, PingReq, PubAck, PubAckProperties, PubAckReason, SubAck, SubscribeReasonCode,
};

use crate::error::{MqttTransportError, PublishRejection};
use crate::link::PublishToken;

/// MQTT5 PUBACK reason code for "Quota exceeded" (0x97).
const PUBACK_QUOTA_EXCEEDED: u8 = 0x97;

/// Classification of an incoming PUBACK against the outstanding publish pkid.
#[derive(Debug)]
pub(crate) enum PubAckMatch {
    /// pkid matched the outstanding publish; consume it and surface this result.
    Matched {
        token: PublishToken,
        result: Result<(), MqttTransportError>,
    },
}

/// Classification of an incoming SUBACK against the pending subscribe pkid.
pub(crate) enum SubAckMatch {
    Matched {
        result: Result<(), MqttTransportError>,
        debug: String,
    },
    Ignored,
}

/// Encode a PUBACK (reason Success) for an incoming QoS1 PUBLISH.
pub(crate) fn encode_puback(pkid: u16) -> Result<Vec<u8>, MqttTransportError> {
    let mut buffer = BytesMut::new();
    PubAck::new(pkid)
        .write(&mut buffer)
        .map_err(|err| MqttTransportError::BrokerDisconnected {
            reason: format!("failed to encode PUBACK: {err:?}"),
        })?;
    Ok(buffer.to_vec())
}

/// Encode a PINGREQ.
pub(crate) fn encode_pingreq() -> Result<Vec<u8>, MqttTransportError> {
    let mut buffer = BytesMut::new();
    PingReq
        .write(&mut buffer)
        .map_err(|err| MqttTransportError::BrokerDisconnected {
            reason: format!("failed to encode PINGREQ: {err:?}"),
        })?;
    Ok(buffer.to_vec())
}

/// Encode a normal DISCONNECT (sent before closing the socket, mirroring the
/// native backend which disconnects gracefully before dropping the connection).
pub(crate) fn encode_disconnect() -> Result<Vec<u8>, MqttTransportError> {
    let mut buffer = BytesMut::new();
    Disconnect::new()
        .write(&mut buffer)
        .map_err(|err| MqttTransportError::BrokerDisconnected {
            reason: format!("failed to encode DISCONNECT: {err:?}"),
        })?;
    Ok(buffer.to_vec())
}

/// Decide the acknowledgement an incoming PUBLISH requires. The raw wasm backend
/// has no auto-ack (unlike rumqttc's `manual_acks = false` default), so a QoS1
/// PUBLISH must be PUBACK'd explicitly or the broker's in-flight window fills and
/// host→client transfers stall. QoS0 needs no ack; QoS2 is never used by Tyde and
/// is treated as a protocol error.
pub(crate) fn incoming_publish_puback(
    qos: QoS,
    pkid: u16,
) -> Result<Option<Vec<u8>>, MqttTransportError> {
    match qos {
        QoS::AtMostOnce => Ok(None),
        QoS::AtLeastOnce => Ok(Some(encode_puback(pkid)?)),
        QoS::ExactlyOnce => Err(MqttTransportError::BrokerDisconnected {
            reason: "received unsupported QoS2 PUBLISH".to_string(),
        }),
    }
}

/// Classify an incoming PUBACK against the publish token the backend already
/// looked up by packet identifier. A PUBACK whose packet id is not outstanding
/// is dropped by the caller before reaching here (we never retransmit, so a
/// stray or duplicate ack is benign and must not fail the link).
pub(crate) fn classify_puback(token: PublishToken, puback: PubAck) -> PubAckMatch {
    PubAckMatch::Matched {
        token,
        result: validate_puback(puback),
    }
}

/// Classify an incoming SUBACK against the pending subscribe pkid.
pub(crate) fn classify_suback(pending: Option<u16>, suback: SubAck) -> SubAckMatch {
    if pending == Some(suback.pkid) {
        let debug = format!("{suback:?}");
        SubAckMatch::Matched {
            result: validate_suback(suback),
            debug,
        }
    } else {
        SubAckMatch::Ignored
    }
}

fn validate_puback(puback: PubAck) -> Result<(), MqttTransportError> {
    match puback.reason {
        PubAckReason::Success => Ok(()),
        reason => Err(MqttTransportError::PublishRejected {
            reason: PublishRejection {
                code: puback_reason_code(reason),
                code_name: format!("{reason:?}"),
                reason_string: puback_reason_string(puback.properties.as_ref()),
            },
        }),
    }
}

fn puback_reason_string(properties: Option<&PubAckProperties>) -> Option<String> {
    properties.and_then(|properties| properties.reason_string.clone())
}

/// Map mqttbytes' `PubAckReason` to its canonical MQTT5 numeric reason code (the
/// driver classifies quota rejections on this code).
fn puback_reason_code(reason: PubAckReason) -> u8 {
    match reason {
        PubAckReason::Success => 0x00,
        PubAckReason::NoMatchingSubscribers => 0x10,
        PubAckReason::UnspecifiedError => 0x80,
        PubAckReason::ImplementationSpecificError => 0x83,
        PubAckReason::NotAuthorized => 0x87,
        PubAckReason::TopicNameInvalid => 0x90,
        PubAckReason::PacketIdentifierInUse => 0x91,
        PubAckReason::QuotaExceeded => PUBACK_QUOTA_EXCEEDED,
        PubAckReason::PayloadFormatInvalid => 0x99,
    }
}

fn validate_suback(suback: SubAck) -> Result<(), MqttTransportError> {
    let mut codes = suback.return_codes.into_iter();
    let first = codes
        .next()
        .ok_or_else(|| MqttTransportError::SubscribeRejected {
            reason: "SUBACK contained no reason codes".to_string(),
        })?;
    if codes.next().is_some() {
        return Err(MqttTransportError::SubscribeRejected {
            reason: "SUBACK contained more reason codes than requested subscriptions".to_string(),
        });
    }
    match first {
        // mqttbytes encodes a granted QoS as QoS0/QoS1/QoS2; we always request
        // QoS1, so only QoS1 is a successful grant.
        SubscribeReasonCode::QoS1 => Ok(()),
        SubscribeReasonCode::QoS0 | SubscribeReasonCode::QoS2 => {
            Err(MqttTransportError::SubscribeRejected {
                reason: format!("broker granted unsupported QoS: {first:?}"),
            })
        }
        reason => Err(MqttTransportError::SubscribeRejected {
            reason: format!("{reason:?}"),
        }),
    }
}
