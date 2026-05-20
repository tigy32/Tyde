use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::config::ParticipantRole;
use crate::error::{CounterViolation, CryptoError};
use crate::framing::{AEAD_KEY_LEN, AEAD_NONCE_LEN, SESSION_SALT_LEN};
use crate::types::{PreSharedKey, RoomId};

pub const HKDF_INFO: &[u8] = b"tyde-mqtt-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterDecision {
    Accept,
    DropDuplicate,
}

#[derive(Debug, Clone, Default)]
pub struct ReceiveCounter {
    last_seen: Option<u64>,
}

impl ReceiveCounter {
    pub fn new() -> Self {
        Self { last_seen: None }
    }

    pub fn validate(&mut self, counter: u64) -> Result<CounterDecision, CryptoError> {
        match self.last_seen {
            None if counter == 0 => {
                self.last_seen = Some(0);
                Ok(CounterDecision::Accept)
            }
            None => Err(CryptoError::CounterViolation(
                CounterViolation::FirstFrameMustBeZero { actual: counter },
            )),
            Some(last_seen) if counter < last_seen => Err(CryptoError::CounterViolation(
                CounterViolation::ReplayedOlderFrame {
                    last_seen,
                    actual: counter,
                },
            )),
            Some(last_seen) if counter == last_seen => Ok(CounterDecision::DropDuplicate),
            Some(last_seen) => match last_seen.checked_add(1) {
                Some(next) if counter == next => {
                    self.last_seen = Some(counter);
                    Ok(CounterDecision::Accept)
                }
                _ => Err(CryptoError::CounterViolation(CounterViolation::Gap {
                    last_seen: Some(last_seen),
                    actual: counter,
                })),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncryptedChunk {
    pub counter: u64,
    pub ciphertext_with_tag: Vec<u8>,
}

pub struct SessionCipher {
    cipher: ChaCha20Poly1305,
    aad: Vec<u8>,
    send_direction: u8,
    recv_direction: u8,
    send_counter: u64,
    recv_counter: ReceiveCounter,
}

impl SessionCipher {
    pub fn new(
        room: &RoomId,
        psk: &PreSharedKey,
        role: ParticipantRole,
        host_salt: &[u8; SESSION_SALT_LEN],
        client_salt: &[u8; SESSION_SALT_LEN],
    ) -> Result<Self, CryptoError> {
        let key = derive_session_key(psk, host_salt, client_salt)?;
        Self::from_key(room, role, &key)
    }

    pub fn from_key(
        room: &RoomId,
        role: ParticipantRole,
        key: &[u8; AEAD_KEY_LEN],
    ) -> Result<Self, CryptoError> {
        let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoError::HkdfExpand)?;
        Ok(Self {
            cipher,
            aad: room_aad(room),
            send_direction: role.outbound_direction(),
            recv_direction: role.inbound_direction(),
            send_counter: 0,
            recv_counter: ReceiveCounter::new(),
        })
    }

    pub fn encrypt_next(&mut self, plaintext: &[u8]) -> Result<EncryptedChunk, CryptoError> {
        let counter = self.send_counter;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterRollover)?;
        let ciphertext_with_tag = encrypt_chunk(
            &self.cipher,
            &self.aad,
            self.send_direction,
            counter,
            plaintext,
        )?;
        Ok(EncryptedChunk {
            counter,
            ciphertext_with_tag,
        })
    }

    pub fn decrypt_received(
        &mut self,
        counter: u64,
        ciphertext_with_tag: &[u8],
    ) -> Result<Option<Vec<u8>>, CryptoError> {
        match self.recv_counter.validate(counter)? {
            CounterDecision::DropDuplicate => Ok(None),
            CounterDecision::Accept => decrypt_chunk(
                &self.cipher,
                &self.aad,
                self.recv_direction,
                counter,
                ciphertext_with_tag,
            )
            .map(Some),
        }
    }

    #[cfg(test)]
    pub(crate) fn encrypt_next_with_direction_for_test(
        &mut self,
        direction: u8,
        plaintext: &[u8],
    ) -> Result<EncryptedChunk, CryptoError> {
        let counter = self.send_counter;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterRollover)?;
        let ciphertext_with_tag =
            encrypt_chunk(&self.cipher, &self.aad, direction, counter, plaintext)?;
        Ok(EncryptedChunk {
            counter,
            ciphertext_with_tag,
        })
    }
}

pub fn derive_session_key(
    psk: &PreSharedKey,
    host_salt: &[u8; SESSION_SALT_LEN],
    client_salt: &[u8; SESSION_SALT_LEN],
) -> Result<[u8; AEAD_KEY_LEN], CryptoError> {
    let mut salt = [0_u8; SESSION_SALT_LEN * 2];
    salt[..SESSION_SALT_LEN].copy_from_slice(host_salt);
    salt[SESSION_SALT_LEN..].copy_from_slice(client_salt);

    let hk = Hkdf::<Sha256>::new(Some(&salt), psk.as_bytes());
    let mut key = [0_u8; AEAD_KEY_LEN];
    hk.expand(HKDF_INFO, &mut key)
        .map_err(|_| CryptoError::HkdfExpand)?;
    Ok(key)
}

/// Returns the AEAD associated data: the canonical base64url-no-pad room string
/// bytes. This is the exact `<room_id>` string used in MQTT topics.
pub fn room_aad(room: &RoomId) -> Vec<u8> {
    room.as_base64url_no_pad().into_bytes()
}

pub fn nonce(direction: u8, counter: u64) -> [u8; AEAD_NONCE_LEN] {
    let mut nonce = [0_u8; AEAD_NONCE_LEN];
    nonce[0] = direction;
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn encrypt_chunk(
    cipher: &ChaCha20Poly1305,
    aad: &[u8],
    direction: u8,
    counter: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce = nonce(direction, counter);
    cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::AeadFailure)
}

fn decrypt_chunk(
    cipher: &ChaCha20Poly1305,
    aad: &[u8],
    direction: u8,
    counter: u64,
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce = nonce(direction, counter);
    cipher
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: ciphertext_with_tag,
                aad,
            },
        )
        .map_err(|_| CryptoError::AeadFailure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParticipantRole;
    use crate::error::CounterViolation;
    use crate::framing::DIRECTION_CLIENT_TO_HOST;

    fn psk() -> PreSharedKey {
        PreSharedKey([3_u8; 32])
    }

    fn room() -> RoomId {
        RoomId([4_u8; 16])
    }

    #[test]
    fn host_and_client_derive_matching_keys() -> Result<(), CryptoError> {
        let host_salt = [1_u8; SESSION_SALT_LEN];
        let client_salt = [2_u8; SESSION_SALT_LEN];
        let host_key = derive_session_key(&psk(), &host_salt, &client_salt)?;
        let client_key = derive_session_key(&psk(), &host_salt, &client_salt)?;
        assert_eq!(host_key, client_key);
        Ok(())
    }

    #[test]
    fn counter_monotonicity_duplicate_and_gap() -> Result<(), CryptoError> {
        let host_salt = [1_u8; SESSION_SALT_LEN];
        let client_salt = [2_u8; SESSION_SALT_LEN];
        let mut sender = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Host,
            &host_salt,
            &client_salt,
        )?;
        let mut receiver = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Client,
            &host_salt,
            &client_salt,
        )?;

        let first = sender.encrypt_next(b"a")?;
        let second = sender.encrypt_next(b"b")?;
        let third = sender.encrypt_next(b"c")?;
        assert_eq!(first.counter, 0);
        assert_eq!(second.counter, 1);
        assert_eq!(third.counter, 2);

        assert_eq!(
            receiver.decrypt_received(first.counter, &first.ciphertext_with_tag)?,
            Some(b"a".to_vec())
        );
        assert_eq!(
            receiver.decrypt_received(first.counter, &first.ciphertext_with_tag)?,
            None
        );
        assert_eq!(
            receiver.decrypt_received(second.counter, &second.ciphertext_with_tag)?,
            Some(b"b".to_vec())
        );
        assert_eq!(
            receiver.decrypt_received(third.counter, &third.ciphertext_with_tag)?,
            Some(b"c".to_vec())
        );

        let gap_ciphertext = sender.encrypt_next(b"gap")?.ciphertext_with_tag;
        let err = receiver.decrypt_received(5, &gap_ciphertext).err();
        assert!(matches!(
            err,
            Some(CryptoError::CounterViolation(CounterViolation::Gap {
                last_seen: Some(2),
                actual: 5
            }))
        ));
        Ok(())
    }

    #[test]
    fn first_frame_counter_must_be_zero() {
        let mut counter = ReceiveCounter::new();
        let err = counter.validate(1).err();
        assert!(matches!(
            err,
            Some(CryptoError::CounterViolation(
                CounterViolation::FirstFrameMustBeZero { actual: 1 }
            ))
        ));
    }

    #[test]
    fn wrong_direction_byte_fails_aead() -> Result<(), CryptoError> {
        let host_salt = [1_u8; SESSION_SALT_LEN];
        let client_salt = [2_u8; SESSION_SALT_LEN];
        let mut malicious_sender = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Host,
            &host_salt,
            &client_salt,
        )?;
        let mut receiver = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Client,
            &host_salt,
            &client_salt,
        )?;
        let chunk = malicious_sender
            .encrypt_next_with_direction_for_test(DIRECTION_CLIENT_TO_HOST, b"wrong direction")?;
        let err = receiver
            .decrypt_received(chunk.counter, &chunk.ciphertext_with_tag)
            .err();
        assert_eq!(err, Some(CryptoError::AeadFailure));
        Ok(())
    }

    #[test]
    fn cross_room_aad_misroute_fails_aead() -> Result<(), CryptoError> {
        let host_salt = [1_u8; SESSION_SALT_LEN];
        let client_salt = [2_u8; SESSION_SALT_LEN];
        let room_a = RoomId([4_u8; 16]);
        let room_b = RoomId([5_u8; 16]);
        let mut sender = SessionCipher::new(
            &room_a,
            &psk(),
            ParticipantRole::Host,
            &host_salt,
            &client_salt,
        )?;
        let mut receiver = SessionCipher::new(
            &room_b,
            &psk(),
            ParticipantRole::Client,
            &host_salt,
            &client_salt,
        )?;
        let chunk = sender.encrypt_next(b"room a")?;
        let err = receiver
            .decrypt_received(chunk.counter, &chunk.ciphertext_with_tag)
            .err();
        assert_eq!(err, Some(CryptoError::AeadFailure));
        Ok(())
    }
}
