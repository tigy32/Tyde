use std::collections::BTreeMap;

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::config::ParticipantRole;
use crate::error::{CounterViolation, CryptoError};
use crate::framing::{AEAD_KEY_LEN, AEAD_NONCE_LEN, SESSION_SALT_LEN};
use crate::link::{DATA_CREDIT_WINDOW, MQTT_QOS1_WINDOW};
use crate::types::{PreSharedKey, RoomId};

pub const HKDF_INFO: &[u8] = b"tyde-mqtt-v1";

/// How far ahead of the next owed frame a data frame may legitimately arrive.
///
/// Data sends are serialized for the beta hotfix, but broker/subscriber QoS-1
/// delivery can still reorder within the MQTT receive headroom. Future frames
/// are buffered only after AEAD succeeds; a gap beyond this bounded headroom is
/// still a fatal transport invariant violation.
pub(crate) const RECEIVE_REORDER_WINDOW: u64 = MQTT_QOS1_WINDOW as u64;
const CREDIT_PLAINTEXT_LEN: usize = 8;
const _: () = assert!(DATA_CREDIT_WINDOW as u64 <= RECEIVE_REORDER_WINDOW);

/// Reassembles the ordered byte stream from data frames.
///
/// This is transport-layer reassembly (like TCP), not "fixing up" the Tyde
/// protocol sequence: the NDJSON `seq` above this layer is still validated
/// strictly. Broker PUBACK is not Tyde receiver credit, so the sender is
/// serialized separately; the receiver still tolerates bounded MQTT reordering
/// and fails loudly for beyond-window gaps.
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

    /// Reject a counter too far ahead to be legitimately buffered.
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

    fn next_expected(&self) -> u64 {
        self.next_expected
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
    send_credit_direction: u8,
    recv_credit_direction: u8,
    send_counter: u64,
    send_credit_counter: u64,
    highest_received_credit_counter: Option<u64>,
    peer_credit_next_expected: u64,
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
            send_credit_direction: role.outbound_credit_direction(),
            recv_credit_direction: role.inbound_credit_direction(),
            send_counter: 0,
            send_credit_counter: 0,
            highest_received_credit_counter: None,
            peer_credit_next_expected: 0,
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

    pub fn encrypt_credit(
        &mut self,
        next_expected_data_counter: u64,
    ) -> Result<EncryptedChunk, CryptoError> {
        let counter = self.send_credit_counter;
        self.send_credit_counter = self
            .send_credit_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterRollover)?;
        let ciphertext_with_tag = encrypt_chunk(
            &self.cipher,
            &self.aad,
            self.send_credit_direction,
            counter,
            &next_expected_data_counter.to_be_bytes(),
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

    pub fn decrypt_credit(
        &mut self,
        control_counter: u64,
        ciphertext_with_tag: &[u8],
    ) -> Result<Option<u64>, CryptoError> {
        let plaintext = decrypt_chunk(
            &self.cipher,
            &self.aad,
            self.recv_credit_direction,
            control_counter,
            ciphertext_with_tag,
        )?;
        let bytes: [u8; CREDIT_PLAINTEXT_LEN] = plaintext
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::AeadFailure)?;
        if self
            .highest_received_credit_counter
            .is_some_and(|highest| control_counter <= highest)
        {
            return Ok(None);
        }
        self.highest_received_credit_counter = Some(control_counter);
        let credit_next_expected = u64::from_be_bytes(bytes);
        if credit_next_expected > self.send_counter {
            return Err(CryptoError::CounterViolation(
                CounterViolation::CreditBeyondSent {
                    sent_next: self.send_counter,
                    credit_next: credit_next_expected,
                },
            ));
        }
        if credit_next_expected <= self.peer_credit_next_expected {
            return Ok(None);
        }
        self.peer_credit_next_expected = credit_next_expected;
        Ok(Some(credit_next_expected))
    }

    pub fn next_send_data_counter(&self) -> u64 {
        self.send_counter
    }

    pub fn peer_credit_next_expected(&self) -> u64 {
        self.peer_credit_next_expected
    }

    pub fn local_next_expected_data_counter(&self) -> u64 {
        self.recv.next_expected()
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
    use crate::framing::{DIRECTION_CLIENT_TO_HOST, DIRECTION_CREDIT_CLIENT_TO_HOST};

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
    fn counter_one_before_zero_buffers_and_drains() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let first = sender.encrypt_next(b"0")?;
        let second = sender.encrypt_next(b"1")?;

        assert!(
            receiver
                .decrypt_received(second.counter, &second.ciphertext_with_tag)?
                .is_empty()
        );
        let delivered = receiver.decrypt_received(first.counter, &first.ciphertext_with_tag)?;
        assert_eq!(delivered, vec![b"0".to_vec(), b"1".to_vec()]);
        Ok(())
    }

    #[test]
    fn buffers_within_window_reorder_and_drains_in_order() -> Result<(), CryptoError> {
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
    fn receive_window_accepts_last_counter_before_boundary() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let frames: Vec<_> = (0..MQTT_QOS1_WINDOW)
            .map(|index| sender.encrypt_next(&[index as u8]))
            .collect::<Result<_, _>>()?;
        let edge = &frames[MQTT_QOS1_WINDOW - 1];

        assert!(
            receiver
                .decrypt_received(edge.counter, &edge.ciphertext_with_tag)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn counter_at_receive_window_boundary_is_fatal() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        for _ in 0..MQTT_QOS1_WINDOW {
            let _ = sender.encrypt_next(b"x")?;
        }
        let beyond = sender.encrypt_next(b"x")?;

        let err = receiver
            .decrypt_received(beyond.counter, &beyond.ciphertext_with_tag)
            .err();
        assert!(matches!(
            err,
            Some(CryptoError::CounterViolation(CounterViolation::Gap {
                last_seen: None,
                actual,
            })) if actual == beyond.counter
        ));
        Ok(())
    }

    #[test]
    fn counter_22_after_6_buffers_and_drains() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let frames: Vec<_> = (0..=22)
            .map(|index| sender.encrypt_next(&[index as u8]))
            .collect::<Result<_, _>>()?;

        for (index, frame) in frames.iter().enumerate().take(7) {
            assert_eq!(
                receiver.decrypt_received(frame.counter, &frame.ciphertext_with_tag)?,
                vec![vec![index as u8]]
            );
        }

        assert!(
            receiver
                .decrypt_received(frames[22].counter, &frames[22].ciphertext_with_tag)?
                .is_empty()
        );

        let mut delivered = Vec::new();
        for frame in frames.iter().take(22).skip(7) {
            delivered.extend(receiver.decrypt_received(frame.counter, &frame.ciphertext_with_tag)?);
        }
        let expected: Vec<Vec<u8>> = (7..=22).map(|index| vec![index as u8]).collect();
        assert_eq!(delivered, expected);
        Ok(())
    }

    #[test]
    fn credit_round_trip_updates_peer_credit() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        for _ in 0..5 {
            let _ = receiver.encrypt_next(b"sent")?;
        }

        let credit = sender.encrypt_credit(5)?;
        assert_eq!(
            receiver.decrypt_credit(credit.counter, &credit.ciphertext_with_tag)?,
            Some(5)
        );
        assert_eq!(receiver.peer_credit_next_expected(), 5);
        Ok(())
    }

    #[test]
    fn duplicate_stale_and_lower_credit_are_noops() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        for _ in 0..3 {
            let _ = receiver.encrypt_next(b"sent")?;
        }

        let first = sender.encrypt_credit(2)?;
        assert_eq!(
            receiver.decrypt_credit(first.counter, &first.ciphertext_with_tag)?,
            Some(2)
        );
        assert_eq!(
            receiver.decrypt_credit(first.counter, &first.ciphertext_with_tag)?,
            None
        );

        let lower = sender.encrypt_credit(1)?;
        assert_eq!(
            receiver.decrypt_credit(lower.counter, &lower.ciphertext_with_tag)?,
            None
        );
        assert_eq!(receiver.peer_credit_next_expected(), 2);
        Ok(())
    }

    #[test]
    fn credit_control_counter_gaps_are_tolerated() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        for _ in 0..4 {
            let _ = receiver.encrypt_next(b"sent")?;
        }

        let first = sender.encrypt_credit(1)?;
        let _skipped = sender.encrypt_credit(2)?;
        let third = sender.encrypt_credit(3)?;

        assert_eq!(
            receiver.decrypt_credit(first.counter, &first.ciphertext_with_tag)?,
            Some(1)
        );
        assert_eq!(
            receiver.decrypt_credit(third.counter, &third.ciphertext_with_tag)?,
            Some(3)
        );
        assert_eq!(receiver.peer_credit_next_expected(), 3);
        Ok(())
    }

    #[test]
    fn future_credit_beyond_sent_is_fatal() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let credit = sender.encrypt_credit(1)?;
        let err = receiver
            .decrypt_credit(credit.counter, &credit.ciphertext_with_tag)
            .err();
        assert!(matches!(
            err,
            Some(CryptoError::CounterViolation(
                CounterViolation::CreditBeyondSent {
                    sent_next: 0,
                    credit_next: 1,
                }
            ))
        ));
        Ok(())
    }

    #[test]
    fn wrong_credit_direction_fails_aead() -> Result<(), CryptoError> {
        let (sender, mut receiver) = sender_receiver()?;
        let ciphertext_with_tag = encrypt_chunk(
            &sender.cipher,
            &sender.aad,
            DIRECTION_CREDIT_CLIENT_TO_HOST,
            0,
            &0_u64.to_be_bytes(),
        )?;

        let err = receiver.decrypt_credit(0, &ciphertext_with_tag).err();
        assert_eq!(err, Some(CryptoError::AeadFailure));
        Ok(())
    }

    #[test]
    fn tampered_credit_fails_aead() -> Result<(), CryptoError> {
        let (mut sender, mut receiver) = sender_receiver()?;
        let mut credit = sender.encrypt_credit(0)?;
        credit.ciphertext_with_tag[0] ^= 0xff;

        let err = receiver
            .decrypt_credit(credit.counter, &credit.ciphertext_with_tag)
            .err();
        assert_eq!(err, Some(CryptoError::AeadFailure));
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
