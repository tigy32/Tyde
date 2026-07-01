use std::collections::BTreeMap;

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::config::ParticipantRole;
use crate::error::{CounterViolation, CryptoError};
use crate::framing::{AEAD_KEY_LEN, AEAD_NONCE_LEN, SESSION_SALT_LEN};
use crate::link::MAX_QOS1_INFLIGHT;
use crate::types::{PreSharedKey, RoomId};

pub const HKDF_INFO: &[u8] = b"tyde-mqtt-v1";

/// How far ahead of the next owed frame a data frame may legitimately arrive.
///
/// The transport keeps up to [`MAX_QOS1_INFLIGHT`] QoS-1 publishes in flight, so
/// the MQTT substrate can reorder them by at most that many counters. A frame
/// more than one full window ahead cannot be a legitimately in-flight publish;
/// it means the sender exceeded the window or the stream lost a frame QoS-1
/// promised to deliver — a real invariant violation the receiver must surface.
const RECEIVE_REORDER_WINDOW: u64 = MAX_QOS1_INFLIGHT as u64;

/// Reassembles the ordered byte stream from data frames the MQTT substrate may
/// deliver out of order, holding early frames until the gap before them fills.
///
/// This is transport-layer reassembly (like TCP), not "fixing up" the Tyde
/// protocol sequence: the NDJSON `seq` above this layer is still validated
/// strictly. Pipelined QoS-1 publishes let the broker deliver frames out of
/// order, and the byte-stream contract requires the receiver to put them back
/// in counter order before handing them up.
#[derive(Debug, Default)]
struct ReceiveReassembler {
    /// Counter of the next frame still owed to the byte stream.
    next_expected: u64,
    /// Decrypted frames received ahead of `next_expected`, keyed by counter.
    pending: BTreeMap<u64, Vec<u8>>,
}

impl ReceiveReassembler {
    fn new() -> Self {
        Self::default()
    }

    /// A frame already delivered or already buffered is a QoS-1 redelivery.
    fn is_duplicate(&self, counter: u64) -> bool {
        counter < self.next_expected || self.pending.contains_key(&counter)
    }

    /// Reject a counter too far ahead to be a legitimately in-flight frame.
    fn ensure_within_window(&self, counter: u64) -> Result<(), CryptoError> {
        let window_end = self.next_expected.saturating_add(RECEIVE_REORDER_WINDOW);
        if counter >= window_end {
            return Err(CryptoError::CounterViolation(CounterViolation::Gap {
                last_seen: self.next_expected.checked_sub(1),
                actual: counter,
            }));
        }
        Ok(())
    }

    /// Buffer a decrypted frame and return the run now deliverable in order
    /// (empty while the gap before `next_expected` remains unfilled).
    fn insert_and_drain(
        &mut self,
        counter: u64,
        plaintext: Vec<u8>,
    ) -> Result<Vec<Vec<u8>>, CryptoError> {
        self.pending.insert(counter, plaintext);
        let mut ready = Vec::new();
        while let Some(plaintext) = self.pending.remove(&self.next_expected) {
            ready.push(plaintext);
            self.next_expected = self
                .next_expected
                .checked_add(1)
                .ok_or(CryptoError::CounterRollover)?;
        }
        Ok(ready)
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
    recv: ReceiveReassembler,
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
            recv: ReceiveReassembler::new(),
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

    /// Decrypt a received data frame and return the run of frames now
    /// deliverable in counter order. A QoS-1 redelivery yields an empty vec, as
    /// does a frame buffered while an earlier one is still outstanding; when the
    /// missing frame arrives it flushes itself and every contiguous successor.
    pub fn decrypt_received(
        &mut self,
        counter: u64,
        ciphertext_with_tag: &[u8],
    ) -> Result<Vec<Vec<u8>>, CryptoError> {
        if self.recv.is_duplicate(counter) {
            return Ok(Vec::new());
        }
        self.recv.ensure_within_window(counter)?;
        let plaintext = decrypt_chunk(
            &self.cipher,
            &self.aad,
            self.recv_direction,
            counter,
            ciphertext_with_tag,
        )?;
        self.recv.insert_and_drain(counter, plaintext)
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

    fn sender_receiver() -> Result<(SessionCipher, SessionCipher), CryptoError> {
        let host_salt = [1_u8; SESSION_SALT_LEN];
        let client_salt = [2_u8; SESSION_SALT_LEN];
        let sender = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Host,
            &host_salt,
            &client_salt,
        )?;
        let receiver = SessionCipher::new(
            &room(),
            &psk(),
            ParticipantRole::Client,
            &host_salt,
            &client_salt,
        )?;
        Ok((sender, receiver))
    }

    #[test]
    fn delivers_in_order_and_drops_duplicates() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let first = sender.encrypt_next(b"a")?;
        let second = sender.encrypt_next(b"b")?;
        let third = sender.encrypt_next(b"c")?;
        assert_eq!(first.counter, 0);
        assert_eq!(second.counter, 1);
        assert_eq!(third.counter, 2);

        assert_eq!(
            receiver.decrypt_received(first.counter, &first.ciphertext_with_tag)?,
            vec![b"a".to_vec()]
        );
        // A QoS-1 redelivery of an already-delivered frame yields nothing.
        assert!(
            receiver
                .decrypt_received(first.counter, &first.ciphertext_with_tag)?
                .is_empty()
        );
        assert_eq!(
            receiver.decrypt_received(second.counter, &second.ciphertext_with_tag)?,
            vec![b"b".to_vec()]
        );
        assert_eq!(
            receiver.decrypt_received(third.counter, &third.ciphertext_with_tag)?,
            vec![b"c".to_vec()]
        );
        // A frame older than what we've already delivered is a duplicate too.
        assert!(
            receiver
                .decrypt_received(second.counter, &second.ciphertext_with_tag)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn buffers_out_of_order_frames_until_the_gap_fills() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let frames: Vec<_> = (0..4)
            .map(|index| sender.encrypt_next(&[b'0' + index as u8]))
            .collect::<Result<_, _>>()?;

        // Frames arrive 2, 1, 3, 0 — nothing is deliverable until 0 fills the gap.
        assert!(
            receiver
                .decrypt_received(frames[2].counter, &frames[2].ciphertext_with_tag)?
                .is_empty()
        );
        assert!(
            receiver
                .decrypt_received(frames[1].counter, &frames[1].ciphertext_with_tag)?
                .is_empty()
        );
        assert!(
            receiver
                .decrypt_received(frames[3].counter, &frames[3].ciphertext_with_tag)?
                .is_empty()
        );
        let delivered =
            receiver.decrypt_received(frames[0].counter, &frames[0].ciphertext_with_tag)?;
        assert_eq!(
            delivered,
            vec![b"0".to_vec(), b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn counter_beyond_the_inflight_window_is_fatal() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        // Advance the sender past a full reorder window without filling the gap.
        let mut far = sender.encrypt_next(b"x")?;
        for _ in 0..20 {
            far = sender.encrypt_next(b"x")?;
        }
        let err = receiver
            .decrypt_received(far.counter, &far.ciphertext_with_tag)
            .err();
        assert!(matches!(
            err,
            Some(CryptoError::CounterViolation(CounterViolation::Gap {
                last_seen: None,
                actual,
            })) if actual == far.counter
        ));
        Ok(())
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
