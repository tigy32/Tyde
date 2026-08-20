use std::collections::BTreeMap;

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::config::ParticipantRole;
use crate::error::{CounterViolation, CryptoError};
use crate::framing::{AEAD_KEY_LEN, AEAD_NONCE_LEN, SESSION_SALT_LEN};
use crate::link::DATA_CREDIT_WINDOW;
use crate::types::{PreSharedKey, RoomId};

pub const HKDF_INFO: &[u8] = b"tyde-mqtt-v1";

/// How far ahead of the next owed frame a data frame may legitimately arrive.
///
/// Data sends are serialized for the beta hotfix, but broker/subscriber QoS-1
/// delivery can still reorder within the MQTT receive headroom. Future frames
/// are buffered only after AEAD succeeds; a gap beyond this bounded headroom is
/// still a fatal transport invariant violation.
///
/// Derived from the credit window rather than [`MQTT_QOS1_WINDOW`], which also
/// sets the broker-facing `receive_maximum` and must stay inside AWS IoT's
/// QoS-1 in-flight cap. Credit is what bounds how far a sender may run ahead,
/// so the buffer only has to cover that plus reordering headroom.
pub(crate) const RECEIVE_REORDER_WINDOW: u64 = (DATA_CREDIT_WINDOW * 2) as u64;
const CREDIT_PLAINTEXT_LEN: usize = 8;
const _: () = assert!(DATA_CREDIT_WINDOW as u64 <= RECEIVE_REORDER_WINDOW);

/// Reassembles the ordered byte stream from data frames.
///
/// This is transport-layer reassembly (like TCP), not "fixing up" the Tyde
/// protocol sequence: the Tyde envelope `seq` above this layer is still validated
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
