use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;

use crate::error::{CryptoError, FramingError};
use crate::framing::{AEAD_KEY_LEN, AEAD_NONCE_LEN, MQTT_TRANSPORT_VERSION};
use crate::types::{PreSharedKey, RoomId};

pub const RENDEZVOUS_OPEN_TAG: u8 = 0x03;
pub const RENDEZVOUS_ACCEPT_TAG: u8 = 0x04;
const CONNECTION_ID_LEN: usize = 16;
const RENDEZVOUS_KEY_INFO: &[u8] = b"tyde-mqtt-rendezvous-v1";
const EPHEMERAL_KEY_INFO: &[u8] = b"tyde-mqtt-ephemeral-data-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionId(pub [u8; CONNECTION_ID_LEN]);

impl ConnectionId {
    pub(crate) fn random() -> Self {
        let mut bytes = [0_u8; CONNECTION_ID_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRequest {
    pub(crate) connection_id: ConnectionId,
    pub(crate) client_nonce: [u8; CONNECTION_ID_LEN],
    pub(crate) proposed_data_room: RoomId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenAccept {
    pub(crate) connection_id: ConnectionId,
    pub(crate) client_nonce: [u8; CONNECTION_ID_LEN],
    pub(crate) server_nonce: [u8; CONNECTION_ID_LEN],
    pub(crate) data_room: RoomId,
}

pub(crate) fn random_nonce() -> [u8; CONNECTION_ID_LEN] {
    let mut bytes = [0_u8; CONNECTION_ID_LEN];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

pub(crate) fn encode_open_request(
    main_room: &RoomId,
    psk: &PreSharedKey,
    request: &OpenRequest,
) -> Result<Vec<u8>, CryptoError> {
    let mut plaintext = Vec::with_capacity(CONNECTION_ID_LEN + crate::types::ROOM_ID_LEN);
    plaintext.extend_from_slice(&request.client_nonce);
    plaintext.extend_from_slice(request.proposed_data_room.as_bytes());
    encode_control_frame(
        main_room,
        psk,
        RENDEZVOUS_OPEN_TAG,
        request.connection_id,
        &plaintext,
    )
}

pub(crate) fn decode_open_request(
    main_room: &RoomId,
    psk: &PreSharedKey,
    payload: &[u8],
) -> Result<OpenRequest, FramingError> {
    let (connection_id, plaintext) =
        decode_control_frame(main_room, psk, RENDEZVOUS_OPEN_TAG, payload)?;
    if plaintext.len() != CONNECTION_ID_LEN + crate::types::ROOM_ID_LEN {
        return Err(FramingError::InvalidRendezvousLength {
            expected: CONNECTION_ID_LEN + crate::types::ROOM_ID_LEN,
            actual: plaintext.len(),
        });
    }
    let mut client_nonce = [0_u8; CONNECTION_ID_LEN];
    client_nonce.copy_from_slice(&plaintext[..CONNECTION_ID_LEN]);
    let mut room = [0_u8; crate::types::ROOM_ID_LEN];
    room.copy_from_slice(&plaintext[CONNECTION_ID_LEN..]);
    Ok(OpenRequest {
        connection_id,
        client_nonce,
        proposed_data_room: RoomId(room),
    })
}

pub(crate) fn encode_open_accept(
    main_room: &RoomId,
    psk: &PreSharedKey,
    accept: &OpenAccept,
) -> Result<Vec<u8>, CryptoError> {
    let mut plaintext = Vec::with_capacity(CONNECTION_ID_LEN * 2 + crate::types::ROOM_ID_LEN);
    plaintext.extend_from_slice(&accept.client_nonce);
    plaintext.extend_from_slice(&accept.server_nonce);
    plaintext.extend_from_slice(accept.data_room.as_bytes());
    encode_control_frame(
        main_room,
        psk,
        RENDEZVOUS_ACCEPT_TAG,
        accept.connection_id,
        &plaintext,
    )
}

pub(crate) fn decode_open_accept(
    main_room: &RoomId,
    psk: &PreSharedKey,
    payload: &[u8],
) -> Result<OpenAccept, FramingError> {
    let (connection_id, plaintext) =
        decode_control_frame(main_room, psk, RENDEZVOUS_ACCEPT_TAG, payload)?;
    if plaintext.len() != CONNECTION_ID_LEN * 2 + crate::types::ROOM_ID_LEN {
        return Err(FramingError::InvalidRendezvousLength {
            expected: CONNECTION_ID_LEN * 2 + crate::types::ROOM_ID_LEN,
            actual: plaintext.len(),
        });
    }
    let mut client_nonce = [0_u8; CONNECTION_ID_LEN];
    client_nonce.copy_from_slice(&plaintext[..CONNECTION_ID_LEN]);
    let mut server_nonce = [0_u8; CONNECTION_ID_LEN];
    server_nonce.copy_from_slice(&plaintext[CONNECTION_ID_LEN..CONNECTION_ID_LEN * 2]);
    let mut room = [0_u8; crate::types::ROOM_ID_LEN];
    room.copy_from_slice(&plaintext[CONNECTION_ID_LEN * 2..]);
    Ok(OpenAccept {
        connection_id,
        client_nonce,
        server_nonce,
        data_room: RoomId(room),
    })
}

pub(crate) fn derive_ephemeral_psk(
    long_term_psk: &PreSharedKey,
    main_room: &RoomId,
    connection_id: ConnectionId,
    client_nonce: &[u8; CONNECTION_ID_LEN],
    server_nonce: &[u8; CONNECTION_ID_LEN],
    data_room: &RoomId,
) -> Result<PreSharedKey, CryptoError> {
    let mut salt = Vec::with_capacity(
        crate::types::ROOM_ID_LEN + CONNECTION_ID_LEN * 3 + crate::types::ROOM_ID_LEN,
    );
    salt.extend_from_slice(main_room.as_bytes());
    salt.extend_from_slice(&connection_id.0);
    salt.extend_from_slice(client_nonce);
    salt.extend_from_slice(server_nonce);
    salt.extend_from_slice(data_room.as_bytes());
    let hk = Hkdf::<Sha256>::new(Some(&salt), long_term_psk.as_bytes());
    let mut key = [0_u8; AEAD_KEY_LEN];
    hk.expand(EPHEMERAL_KEY_INFO, &mut key)
        .map_err(|_| CryptoError::HkdfExpand)?;
    Ok(PreSharedKey(key))
}

fn encode_control_frame(
    main_room: &RoomId,
    psk: &PreSharedKey,
    tag: u8,
    connection_id: ConnectionId,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let key = derive_rendezvous_key(main_room, psk)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key).map_err(|_| CryptoError::HkdfExpand)?;
    let nonce = rendezvous_nonce(tag, connection_id);
    let aad = rendezvous_aad(main_room, tag, connection_id);
    let ciphertext_with_tag = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::AeadFailure)?;
    let mut frame = Vec::with_capacity(2 + CONNECTION_ID_LEN + ciphertext_with_tag.len());
    frame.push(MQTT_TRANSPORT_VERSION);
    frame.push(tag);
    frame.extend_from_slice(&connection_id.0);
    frame.extend_from_slice(&ciphertext_with_tag);
    Ok(frame)
}

fn decode_control_frame(
    main_room: &RoomId,
    psk: &PreSharedKey,
    expected_tag: u8,
    payload: &[u8],
) -> Result<(ConnectionId, Vec<u8>), FramingError> {
    let version = payload.first().copied().ok_or(FramingError::EmptyFrame)?;
    if version != MQTT_TRANSPORT_VERSION {
        return Err(FramingError::VersionMismatch {
            expected: MQTT_TRANSPORT_VERSION,
            actual: version,
        });
    }
    let tag = payload
        .get(1)
        .copied()
        .ok_or(FramingError::DataFrameTooShort {
            minimum: 2,
            actual: payload.len(),
        })?;
    if tag != expected_tag {
        return Err(FramingError::UnknownTag { tag });
    }
    let minimum = 2 + CONNECTION_ID_LEN + crate::framing::AEAD_TAG_LEN;
    if payload.len() < minimum {
        return Err(FramingError::DataFrameTooShort {
            minimum,
            actual: payload.len(),
        });
    }
    let mut id = [0_u8; CONNECTION_ID_LEN];
    id.copy_from_slice(&payload[2..2 + CONNECTION_ID_LEN]);
    let connection_id = ConnectionId(id);
    let key = derive_rendezvous_key(main_room, psk).map_err(FramingError::Crypto)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| FramingError::Crypto(CryptoError::HkdfExpand))?;
    let nonce = rendezvous_nonce(tag, connection_id);
    let aad = rendezvous_aad(main_room, tag, connection_id);
    let plaintext = cipher
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: &payload[2 + CONNECTION_ID_LEN..],
                aad: &aad,
            },
        )
        .map_err(|_| FramingError::Crypto(CryptoError::AeadFailure))?;
    Ok((connection_id, plaintext))
}

fn derive_rendezvous_key(
    main_room: &RoomId,
    psk: &PreSharedKey,
) -> Result<[u8; AEAD_KEY_LEN], CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(main_room.as_bytes()), psk.as_bytes());
    let mut key = [0_u8; AEAD_KEY_LEN];
    hk.expand(RENDEZVOUS_KEY_INFO, &mut key)
        .map_err(|_| CryptoError::HkdfExpand)?;
    Ok(key)
}

fn rendezvous_nonce(tag: u8, connection_id: ConnectionId) -> [u8; AEAD_NONCE_LEN] {
    let mut nonce = [0_u8; AEAD_NONCE_LEN];
    nonce[0] = tag;
    nonce[1..].copy_from_slice(&connection_id.0[..AEAD_NONCE_LEN - 1]);
    nonce
}

fn rendezvous_aad(main_room: &RoomId, tag: u8, connection_id: ConnectionId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        RENDEZVOUS_KEY_INFO.len() + crate::types::ROOM_ID_LEN + 1 + CONNECTION_ID_LEN,
    );
    aad.extend_from_slice(RENDEZVOUS_KEY_INFO);
    aad.extend_from_slice(main_room.as_bytes());
    aad.push(tag);
    aad.extend_from_slice(&connection_id.0);
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let room = RoomId([1_u8; crate::types::ROOM_ID_LEN]);
        let psk = PreSharedKey([2_u8; crate::types::PRE_SHARED_KEY_LEN]);
        let request = OpenRequest {
            connection_id: ConnectionId([3_u8; CONNECTION_ID_LEN]),
            client_nonce: [4_u8; CONNECTION_ID_LEN],
            proposed_data_room: RoomId([5_u8; crate::types::ROOM_ID_LEN]),
        };
        let encoded = encode_open_request(&room, &psk, &request)?;
        assert_eq!(decode_open_request(&room, &psk, &encoded)?, request);
        Ok(())
    }

    #[test]
    fn accept_round_trip_and_ephemeral_key_match() -> Result<(), Box<dyn std::error::Error>> {
        let room = RoomId([1_u8; crate::types::ROOM_ID_LEN]);
        let psk = PreSharedKey([2_u8; crate::types::PRE_SHARED_KEY_LEN]);
        let accept = OpenAccept {
            connection_id: ConnectionId([3_u8; CONNECTION_ID_LEN]),
            client_nonce: [4_u8; CONNECTION_ID_LEN],
            server_nonce: [5_u8; CONNECTION_ID_LEN],
            data_room: RoomId([6_u8; crate::types::ROOM_ID_LEN]),
        };
        let encoded = encode_open_accept(&room, &psk, &accept)?;
        let decoded = decode_open_accept(&room, &psk, &encoded)?;
        assert_eq!(decoded, accept);
        let host = derive_ephemeral_psk(
            &psk,
            &room,
            accept.connection_id,
            &accept.client_nonce,
            &accept.server_nonce,
            &accept.data_room,
        )?;
        let client = derive_ephemeral_psk(
            &psk,
            &room,
            decoded.connection_id,
            &decoded.client_nonce,
            &decoded.server_nonce,
            &decoded.data_room,
        )?;
        assert_eq!(host, client);
        Ok(())
    }
}
