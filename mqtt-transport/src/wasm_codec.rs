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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MqttTransportError;
    use mqttbytes::v5::{Packet, SubAck, SubscribeReasonCode, read};

    fn decode(bytes: &[u8]) -> Packet {
        let mut buffer = BytesMut::from(bytes);
        read(&mut buffer, 64 * 1024).expect("decode packet")
    }

    #[test]
    fn qos1_publish_requires_a_matching_puback() {
        let ack = incoming_publish_puback(QoS::AtLeastOnce, 42)
            .expect("ack decision")
            .expect("QoS1 must produce a PUBACK");
        match decode(&ack) {
            Packet::PubAck(puback) => assert_eq!(puback.pkid, 42),
            other => panic!("expected PUBACK, got {other:?}"),
        }
    }

    #[test]
    fn qos0_publish_requires_no_ack() {
        assert!(
            incoming_publish_puback(QoS::AtMostOnce, 0)
                .expect("ack decision")
                .is_none()
        );
    }

    #[test]
    fn qos2_publish_is_a_protocol_error() {
        assert!(matches!(
            incoming_publish_puback(QoS::ExactlyOnce, 1),
            Err(MqttTransportError::BrokerDisconnected { .. })
        ));
    }

    #[test]
    fn looked_up_puback_token_is_accepted() {
        let token = PublishToken::new(17);
        assert!(matches!(
            classify_puback(token, PubAck::new(7)),
            PubAckMatch::Matched {
                token: matched,
                result: Ok(())
            } if matched == token
        ));
    }

    #[test]
    fn quota_exceeded_puback_is_classified_for_pacing() {
        let mut puback = PubAck::new(3);
        puback.reason = PubAckReason::QuotaExceeded;
        match classify_puback(PublishToken::new(3), puback) {
            PubAckMatch::Matched {
                result: Err(MqttTransportError::PublishRejected { reason }),
                ..
            } => {
                assert!(reason.is_quota_exceeded(), "quota code must drive pacing");
            }
            _ => panic!("expected a matched quota rejection"),
        }
    }

    #[test]
    fn matching_suback_pkid_is_accepted_mismatch_is_ignored() {
        let granted = SubAck::new(5, vec![SubscribeReasonCode::QoS1]);
        match classify_suback(Some(5), granted) {
            SubAckMatch::Matched { result, debug } => {
                result.expect("QoS1 grant is success");
                assert!(
                    !debug.is_empty(),
                    "debug rendering is surfaced to the driver"
                );
            }
            SubAckMatch::Ignored => panic!("matching SUBACK pkid must be accepted"),
        }

        let stray = SubAck::new(6, vec![SubscribeReasonCode::QoS1]);
        assert!(matches!(
            classify_suback(Some(5), stray),
            SubAckMatch::Ignored
        ));
    }

    #[test]
    fn pingreq_and_disconnect_encode_to_expected_packet_types() {
        // PINGREQ round-trips through mqttbytes' own reader.
        assert!(matches!(
            decode(&encode_pingreq().unwrap()),
            Packet::PingReq
        ));
        // A minimal DISCONNECT (remaining length 0) is valid on the wire, but
        // mqttbytes' reader rejects zero-length non-ping packets, so assert on
        // the MQTT control-packet type nibble (DISCONNECT = 14) instead.
        let disconnect = encode_disconnect().unwrap();
        assert_eq!(
            disconnect.first().map(|byte| byte >> 4),
            Some(14),
            "first byte must carry the DISCONNECT packet type"
        );
    }
}
