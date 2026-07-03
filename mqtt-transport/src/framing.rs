use crate::error::FramingError;

pub const MQTT_TRANSPORT_VERSION: u8 = 0x02;
pub const HANDSHAKE_TAG: u8 = 0x01;
pub const DATA_TAG: u8 = 0x02;
pub const CREDIT_TAG: u8 = 0x05;
pub const SESSION_SALT_LEN: usize = 16;
pub const AEAD_NONCE_LEN: usize = 12;
pub const AEAD_KEY_LEN: usize = 32;
pub const DATA_COUNTER_LEN: usize = 8;
pub const CONTROL_COUNTER_LEN: usize = 8;
pub const AEAD_TAG_LEN: usize = 16;
pub const DIRECTION_HOST_TO_CLIENT: u8 = 0x00;
pub const DIRECTION_CLIENT_TO_HOST: u8 = 0x01;
pub const DIRECTION_CREDIT_HOST_TO_CLIENT: u8 = 0x02;
pub const DIRECTION_CREDIT_CLIENT_TO_HOST: u8 = 0x03;

const HEADER_LEN: usize = 2;
const HANDSHAKE_FRAME_LEN: usize = HEADER_LEN + SESSION_SALT_LEN;
const DATA_FRAME_MIN_LEN: usize = HEADER_LEN + DATA_COUNTER_LEN + AEAD_TAG_LEN;
const CREDIT_FRAME_MIN_LEN: usize = HEADER_LEN + CONTROL_COUNTER_LEN + AEAD_TAG_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFrame {
    Handshake {
        salt: [u8; SESSION_SALT_LEN],
    },
    Data {
        counter: u64,
        ciphertext_with_tag: Vec<u8>,
    },
    Credit {
        control_counter: u64,
        ciphertext_with_tag: Vec<u8>,
    },
}

pub fn encode_handshake_frame(salt: &[u8; SESSION_SALT_LEN]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HANDSHAKE_FRAME_LEN);
    frame.push(MQTT_TRANSPORT_VERSION);
    frame.push(HANDSHAKE_TAG);
    frame.extend_from_slice(salt);
    frame
}

pub fn encode_data_frame(counter: u64, ciphertext_with_tag: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + DATA_COUNTER_LEN + ciphertext_with_tag.len());
    frame.push(MQTT_TRANSPORT_VERSION);
    frame.push(DATA_TAG);
    frame.extend_from_slice(&counter.to_be_bytes());
    frame.extend_from_slice(ciphertext_with_tag);
    frame
}

pub fn encode_credit_frame(control_counter: u64, ciphertext_with_tag: &[u8]) -> Vec<u8> {
    let mut frame =
        Vec::with_capacity(HEADER_LEN + CONTROL_COUNTER_LEN + ciphertext_with_tag.len());
    frame.push(MQTT_TRANSPORT_VERSION);
    frame.push(CREDIT_TAG);
    frame.extend_from_slice(&control_counter.to_be_bytes());
    frame.extend_from_slice(ciphertext_with_tag);
    frame
}

pub fn decode_frame(payload: &[u8]) -> Result<TransportFrame, FramingError> {
    let version = match payload.first().copied() {
        Some(version) => version,
        None => return Err(FramingError::EmptyFrame),
    };

    if version != MQTT_TRANSPORT_VERSION {
        return Err(FramingError::VersionMismatch {
            expected: MQTT_TRANSPORT_VERSION,
            actual: version,
        });
    }

    let tag = match payload.get(1).copied() {
        Some(tag) => tag,
        None => {
            return Err(FramingError::DataFrameTooShort {
                minimum: HEADER_LEN,
                actual: payload.len(),
            });
        }
    };

    match tag {
        HANDSHAKE_TAG => decode_handshake_frame(payload),
        DATA_TAG => decode_data_frame(payload),
        CREDIT_TAG => decode_credit_frame(payload),
        tag => Err(FramingError::UnknownTag { tag }),
    }
}

fn decode_handshake_frame(payload: &[u8]) -> Result<TransportFrame, FramingError> {
    if payload.len() != HANDSHAKE_FRAME_LEN {
        return Err(FramingError::InvalidHandshakeLength {
            expected: HANDSHAKE_FRAME_LEN,
            actual: payload.len(),
        });
    }

    let mut salt = [0_u8; SESSION_SALT_LEN];
    salt.copy_from_slice(&payload[HEADER_LEN..HANDSHAKE_FRAME_LEN]);
    Ok(TransportFrame::Handshake { salt })
}

fn decode_data_frame(payload: &[u8]) -> Result<TransportFrame, FramingError> {
    if payload.len() < DATA_FRAME_MIN_LEN {
        return Err(FramingError::DataFrameTooShort {
            minimum: DATA_FRAME_MIN_LEN,
            actual: payload.len(),
        });
    }

    let counter_start = HEADER_LEN;
    let counter_end = counter_start + DATA_COUNTER_LEN;
    let mut counter_bytes = [0_u8; DATA_COUNTER_LEN];
    counter_bytes.copy_from_slice(&payload[counter_start..counter_end]);
    let counter = u64::from_be_bytes(counter_bytes);
    let ciphertext_with_tag = payload[counter_end..].to_vec();

    Ok(TransportFrame::Data {
        counter,
        ciphertext_with_tag,
    })
}

fn decode_credit_frame(payload: &[u8]) -> Result<TransportFrame, FramingError> {
    if payload.len() < CREDIT_FRAME_MIN_LEN {
        return Err(FramingError::DataFrameTooShort {
            minimum: CREDIT_FRAME_MIN_LEN,
            actual: payload.len(),
        });
    }

    let counter_start = HEADER_LEN;
    let counter_end = counter_start + CONTROL_COUNTER_LEN;
    let mut counter_bytes = [0_u8; CONTROL_COUNTER_LEN];
    counter_bytes.copy_from_slice(&payload[counter_start..counter_end]);
    let control_counter = u64::from_be_bytes(counter_bytes);
    let ciphertext_with_tag = payload[counter_end..].to_vec();

    Ok(TransportFrame::Credit {
        control_counter,
        ciphertext_with_tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() -> Result<(), FramingError> {
        let salt = [9_u8; SESSION_SALT_LEN];
        let frame = encode_handshake_frame(&salt);
        assert_eq!(decode_frame(&frame)?, TransportFrame::Handshake { salt });
        Ok(())
    }

    #[test]
    fn rejects_wrong_version() {
        let err = decode_frame(&[0x01, HANDSHAKE_TAG]).err();
        assert!(matches!(err, Some(FramingError::VersionMismatch { .. })));
    }

    #[test]
    fn credit_round_trip() -> Result<(), FramingError> {
        let ciphertext_with_tag = vec![7_u8; AEAD_TAG_LEN + 8];
        let frame = encode_credit_frame(42, &ciphertext_with_tag);
        assert_eq!(
            decode_frame(&frame)?,
            TransportFrame::Credit {
                control_counter: 42,
                ciphertext_with_tag,
            }
        );
        Ok(())
    }

    #[test]
    fn rejects_short_credit_frame() {
        let err = decode_frame(&[MQTT_TRANSPORT_VERSION, CREDIT_TAG, 0]).err();
        assert!(matches!(err, Some(FramingError::DataFrameTooShort { .. })));
    }
}
