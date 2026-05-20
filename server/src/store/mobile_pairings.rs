use std::path::{Path, PathBuf};

use blake2::{Blake2s256, Digest};
use mqtt_transport::{BrokerEndpoint, PreSharedKey, RoomId, validate_broker_url};
use protocol::{MobileDeviceId, MobileDeviceState, MobileDeviceSummary, MobilePairingOfferId};
use serde::{Deserialize, Serialize};

use crate::store::permissions::{atomic_write_owner_only, enforce_owner_only_file};

pub const MOBILE_PAIRINGS_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairings {
    pub version: u32,
    #[serde(default)]
    pub active_pairing: Option<ActiveMobilePairingCredential>,
    #[serde(default)]
    pub devices: Vec<MobilePairingRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveMobilePairingCredential {
    pub offer_id: MobilePairingOfferId,
    pub broker: BrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub created_at_ms: u64,
    pub key_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairingRecord {
    pub device_id: MobileDeviceId,
    pub broker: BrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub label: String,
    pub created_at_ms: u64,
    pub last_seen_at_ms: Option<u64>,
    pub state: MobileDeviceState,
    pub key_fingerprint: String,
}

impl ActiveMobilePairingCredential {
    pub fn key_fingerprint(&self) -> String {
        key_fingerprint(&self.psk)
    }
}

impl MobilePairingRecord {
    pub fn key_fingerprint(&self) -> String {
        key_fingerprint(&self.psk)
    }
}

impl MobilePairings {
    pub fn empty() -> Self {
        Self {
            version: MOBILE_PAIRINGS_VERSION,
            active_pairing: None,
            devices: Vec::new(),
        }
    }

    pub fn summaries(&self) -> Vec<MobileDeviceSummary> {
        self.devices
            .iter()
            .map(|record| MobileDeviceSummary {
                device_id: record.device_id.clone(),
                label: record.label.clone(),
                key_fingerprint: record.key_fingerprint.clone(),
                created_at_ms: record.created_at_ms,
                last_seen_at_ms: record.last_seen_at_ms,
                state: record.state,
            })
            .collect()
    }

    pub fn normalize_startup_runtime_state(&mut self) -> bool {
        let mut changed = self.active_pairing.take().is_some();
        for device in &mut self.devices {
            if device.state == MobileDeviceState::Connected {
                device.state = MobileDeviceState::Paired;
                changed = true;
            }
        }
        changed
    }
}

#[derive(Debug)]
pub struct MobilePairingsStore {
    path: PathBuf,
}

impl MobilePairingsStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        if path.exists() {
            let _ = Self::read_from_disk(&path).map_err(|error| error.message)?;
        }
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home)
            .join(".tyde")
            .join("mobile_pairings.json"))
    }

    pub fn get(&self) -> Result<MobilePairings, String> {
        match Self::read_from_disk(&self.path) {
            Ok(pairings) => Ok(pairings),
            Err(error) if error.kind == StoreReadErrorKind::NotFound => Ok(MobilePairings::empty()),
            Err(error) => Err(error.message),
        }
    }

    pub fn save(&self, pairings: &MobilePairings) -> Result<(), String> {
        validate_pairings(pairings)?;
        let json = serde_json::to_vec_pretty(pairings)
            .map_err(|err| format!("failed to serialize mobile pairings store: {err}"))?;
        atomic_write_owner_only(&self.path, &json)
    }

    fn read_from_disk(path: &Path) -> Result<MobilePairings, StoreReadError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                enforce_owner_only_file(path).map_err(StoreReadError::read)?;
                let pairings =
                    serde_json::from_str::<MobilePairings>(&contents).map_err(|err| {
                        StoreReadError::read(format!(
                            "failed to parse mobile pairings store {}: {err}",
                            path.display()
                        ))
                    })?;
                validate_pairings(&pairings).map_err(StoreReadError::read)?;
                Ok(pairings)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreReadError::not_found(format!(
                    "mobile pairings store {} does not exist",
                    path.display()
                )))
            }
            Err(err) => Err(StoreReadError::read(format!(
                "failed to read mobile pairings store {}: {err}",
                path.display()
            ))),
        }
    }
}

pub fn key_fingerprint(psk: &PreSharedKey) -> String {
    let digest = Blake2s256::digest(psk.as_bytes());
    let truncated = &digest[..16];
    let mut output = String::with_capacity(truncated.len() * 2);
    for byte in truncated {
        push_lower_hex(&mut output, *byte);
    }
    output
}

fn validate_pairings(pairings: &MobilePairings) -> Result<(), String> {
    if pairings.version != MOBILE_PAIRINGS_VERSION {
        return Err(format!(
            "unsupported mobile pairings store version {}, expected {}",
            pairings.version, MOBILE_PAIRINGS_VERSION
        ));
    }
    if let Some(active) = &pairings.active_pairing {
        validate_active(active)?;
    }
    for record in &pairings.devices {
        validate_device(record)?;
    }
    Ok(())
}

fn validate_active(active: &ActiveMobilePairingCredential) -> Result<(), String> {
    if active.offer_id.0.is_empty() {
        return Err("active mobile pairing offer_id must not be empty".to_owned());
    }
    validate_broker_url(&active.broker.url).map_err(|err| err.to_string())?;
    let expected = key_fingerprint(&active.psk);
    if active.key_fingerprint != expected {
        return Err(format!(
            "active mobile pairing {} key_fingerprint does not match psk",
            active.offer_id
        ));
    }
    Ok(())
}

fn validate_device(record: &MobilePairingRecord) -> Result<(), String> {
    if record.device_id.0.is_empty() {
        return Err("mobile pairing device_id must not be empty".to_owned());
    }
    if record.label.trim().is_empty() {
        return Err(format!(
            "mobile pairing {} label must not be empty",
            record.device_id
        ));
    }
    validate_broker_url(&record.broker.url).map_err(|err| err.to_string())?;
    let expected = key_fingerprint(&record.psk);
    if record.key_fingerprint != expected {
        return Err(format!(
            "mobile pairing {} key_fingerprint does not match psk",
            record.device_id
        ));
    }
    Ok(())
}

fn push_lower_hex(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(char::from(HEX[(byte >> 4) as usize]));
    output.push(char::from(HEX[(byte & 0x0f) as usize]));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreReadErrorKind {
    NotFound,
    Read,
}

#[derive(Debug)]
struct StoreReadError {
    kind: StoreReadErrorKind,
    message: String,
}

impl StoreReadError {
    fn not_found(message: String) -> Self {
        Self {
            kind: StoreReadErrorKind::NotFound,
            message,
        }
    }

    fn read(message: String) -> Self {
        Self {
            kind: StoreReadErrorKind::Read,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mqtt_transport::{BrokerAuth, BrokerEndpoint};
    use protocol::BrokerUrl;

    fn endpoint() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://broker.emqx.io:8883").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        }
    }

    #[test]
    fn pairings_round_trip_and_fingerprint_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let store = MobilePairingsStore::load(path).expect("load store");
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        let pairings = MobilePairings {
            version: MOBILE_PAIRINGS_VERSION,
            active_pairing: Some(ActiveMobilePairingCredential {
                offer_id: MobilePairingOfferId::new("offer-1").expect("offer id"),
                broker: endpoint(),
                room: RoomId([7_u8; 16]),
                psk: psk.clone(),
                created_at_ms: 1,
                key_fingerprint: key_fingerprint(&psk),
            }),
            devices: vec![MobilePairingRecord {
                device_id: MobileDeviceId("device-1".to_owned()),
                broker: endpoint(),
                room: RoomId([8_u8; 16]),
                psk: psk.clone(),
                label: "Mike's iPhone".to_owned(),
                created_at_ms: 1,
                last_seen_at_ms: None,
                state: MobileDeviceState::Paired,
                key_fingerprint: key_fingerprint(&psk),
            }],
        };

        store.save(&pairings).expect("save pairings");
        let loaded = store.get().expect("load pairings");
        assert_eq!(loaded, pairings);
    }
}
