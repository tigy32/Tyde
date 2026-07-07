use std::fmt;
use std::path::{Path, PathBuf};

use blake2::{Blake2s256, Digest};
use mqtt_transport::{BrokerEndpoint, PreSharedKey, RoomId, validate_broker_url};
use protocol::{
    ManagedBrokerCredentials, ManagedBrokerEndpoint, MobileDeviceId, MobileDeviceState,
    MobileDeviceSummary, MobilePairingOfferId,
};
use serde::{Deserialize, Serialize};

use crate::store::permissions::{atomic_write_owner_only, enforce_owner_only_file};

pub const MOBILE_PAIRINGS_VERSION: u32 = 2;
pub const MOBILE_PAIRINGS_STORE_PATH_ENV: &str = "TYDE_MOBILE_PAIRINGS_STORE_PATH";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairings {
    pub version: u32,
    #[serde(default, skip_serializing)]
    pub active_pairing: Option<ActiveMobilePairingCredential>,
    #[serde(default)]
    pub devices: Vec<MobilePairingRecord>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveMobilePairingCredential {
    pub offer_id: MobilePairingOfferId,
    pub broker: BrokerEndpoint,
    pub room: RoomId,
    pub psk: PreSharedKey,
    pub created_at_ms: u64,
    pub key_fingerprint: String,
    #[serde(default, skip_serializing)]
    pub managed: Option<ActiveManagedMobilePairingCredential>,
}

impl fmt::Debug for ActiveMobilePairingCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveMobilePairingCredential")
            .field("offer_id", &self.offer_id)
            .field("broker", &self.broker)
            .field("room", &self.room)
            .field("psk", &"<redacted>")
            .field("created_at_ms", &self.created_at_ms)
            .field("key_fingerprint", &self.key_fingerprint)
            .field("managed", &self.managed)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedMobilePairingCredential>,
}

impl fmt::Debug for MobilePairingRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MobilePairingRecord")
            .field("device_id", &self.device_id)
            .field("broker", &self.broker)
            .field("room", &self.room)
            .field("psk", &"<redacted>")
            .field("label", &self.label)
            .field("created_at_ms", &self.created_at_ms)
            .field("last_seen_at_ms", &self.last_seen_at_ms)
            .field("state", &self.state)
            .field("key_fingerprint", &self.key_fingerprint)
            .field("managed", &self.managed)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ActiveManagedMobilePairingCredential {
    pub host_offer_token: String,
    pub pairing_url: String,
    pub broker: ManagedBrokerEndpoint,
    pub host_broker_credentials: ManagedBrokerCredentials,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<ManagedMobilePairingHandoff>,
}

impl fmt::Debug for ActiveManagedMobilePairingCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveManagedMobilePairingCredential")
            .field("host_offer_token", &"<redacted>")
            .field("pairing_url", &"<redacted>")
            .field("broker", &self.broker)
            .field("host_broker_credentials", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("handoff", &self.handoff)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ManagedMobilePairingHandoff {
    pub pairing_id: String,
    pub host_pairing_secret: String,
    pub device_id: MobileDeviceId,
    pub device_label: String,
    pub device_created_at_ms: u64,
    pub device_last_seen_at_ms: Option<u64>,
    pub broker: ManagedBrokerEndpoint,
    pub host_broker_credentials: ManagedBrokerCredentials,
}

impl fmt::Debug for ManagedMobilePairingHandoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedMobilePairingHandoff")
            .field("pairing_id", &self.pairing_id)
            .field("host_pairing_secret", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("device_label", &self.device_label)
            .field("device_created_at_ms", &self.device_created_at_ms)
            .field("device_last_seen_at_ms", &self.device_last_seen_at_ms)
            .field("broker", &self.broker)
            .field("host_broker_credentials", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMobilePairingCredential {
    pub pairing_id: String,
    pub host_pairing_secret: String,
    pub broker: ManagedBrokerEndpoint,
}

impl fmt::Debug for ManagedMobilePairingCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedMobilePairingCredential")
            .field("pairing_id", &self.pairing_id)
            .field("host_pairing_secret", &"<redacted>")
            .field("broker", &self.broker)
            .finish()
    }
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
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Some(path) = mobile_pairings_path_override() {
            return Ok(path);
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("mobile_pairings.json"))
    }

    pub fn path_for_store_parent(parent: &Path) -> PathBuf {
        mobile_pairings_path_override().unwrap_or_else(|| parent.join("mobile_pairings.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> Result<MobilePairings, String> {
        match Self::read_from_disk(&self.path) {
            Ok(pairings) => Ok(pairings),
            Err(error) if error.kind == StoreReadErrorKind::NotFound => Ok(MobilePairings::empty()),
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error.message,
                    "mobile pairings store is unreadable; starting with repair-required state"
                );
                Ok(
                    Self::repairable_pairings_from_disk(&self.path).unwrap_or_else(|| {
                        tracing::warn!(
                            path = %self.path.display(),
                            "mobile pairings store could not be migrated for repair; starting empty"
                        );
                        MobilePairings::empty()
                    }),
                )
            }
        }
    }

    pub fn save(&self, pairings: &MobilePairings) -> Result<(), String> {
        validate_pairings(pairings)?;
        let json = serde_json::to_vec_pretty(pairings)
            .map_err(|err| format!("failed to serialize mobile pairings store: {err}"))?;
        atomic_write_owner_only(&self.path, &json)
    }

    fn read_from_disk(path: &Path) -> Result<MobilePairings, StoreReadError> {
        let pairings = Self::read_unvalidated_from_disk(path)?;
        validate_pairings(&pairings).map_err(StoreReadError::read)?;
        Ok(pairings)
    }

    fn read_unvalidated_from_disk(path: &Path) -> Result<MobilePairings, StoreReadError> {
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

    fn repairable_pairings_from_disk(path: &Path) -> Option<MobilePairings> {
        let mut pairings = Self::read_unvalidated_from_disk(path).ok()?;
        pairings.version = MOBILE_PAIRINGS_VERSION;
        pairings.active_pairing = None;
        for record in &mut pairings.devices {
            record.state = MobileDeviceState::RepairRequired;
            record.managed = None;
            record.key_fingerprint = record.key_fingerprint();
        }
        validate_pairings(&pairings).ok()?;
        Some(pairings)
    }
}

fn mobile_pairings_path_override() -> Option<PathBuf> {
    let path = std::env::var(MOBILE_PAIRINGS_STORE_PATH_ENV).ok()?;
    let trimmed = path.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
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
    if let Some(managed) = &active.managed {
        validate_active_managed(managed)?;
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
    if let Some(managed) = &record.managed {
        validate_managed_pairing(managed)?;
    }
    Ok(())
}

fn validate_active_managed(managed: &ActiveManagedMobilePairingCredential) -> Result<(), String> {
    validate_non_empty("active mobile host_offer_token", &managed.host_offer_token)?;
    validate_non_empty("active mobile pairing_url", &managed.pairing_url)?;
    validate_managed_broker(&managed.broker)?;
    if managed.expires_at_ms == 0 {
        return Err("active managed mobile pairing expires_at_ms must not be zero".to_owned());
    }
    if let Some(handoff) = &managed.handoff {
        validate_managed_handoff(handoff)?;
    }
    Ok(())
}

fn validate_managed_handoff(handoff: &ManagedMobilePairingHandoff) -> Result<(), String> {
    validate_non_empty("managed mobile pairing_id", &handoff.pairing_id)?;
    validate_non_empty(
        "managed mobile host_pairing_secret",
        &handoff.host_pairing_secret,
    )?;
    if handoff.device_id.0.is_empty() {
        return Err("managed mobile handoff device_id must not be empty".to_owned());
    }
    validate_non_empty("managed mobile handoff device_label", &handoff.device_label)?;
    if handoff.device_created_at_ms == 0 {
        return Err("managed mobile handoff device_created_at_ms must not be zero".to_owned());
    }
    validate_managed_broker(&handoff.broker)
}

fn validate_managed_pairing(managed: &ManagedMobilePairingCredential) -> Result<(), String> {
    validate_non_empty("managed mobile pairing_id", &managed.pairing_id)?;
    validate_non_empty(
        "managed mobile host_pairing_secret",
        &managed.host_pairing_secret,
    )?;
    validate_managed_broker(&managed.broker)
}

fn validate_managed_broker(broker: &ManagedBrokerEndpoint) -> Result<(), String> {
    validate_broker_url(&broker.endpoint).map_err(|err| err.to_string())
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
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
    use protocol::{
        BrokerUrl, ManagedBrokerAuthorizerName, ManagedBrokerClientId, ManagedBrokerConnectAuth,
        ManagedBrokerCredentialScope, ManagedBrokerGrantId, ManagedBrokerProvider,
        ManagedBrokerRegion, ManagedBrokerRole, ManagedBrokerTopicNamespace,
    };
    use std::collections::BTreeMap;

    fn endpoint() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://broker.emqx.io:8883").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        }
    }

    fn managed_broker() -> ManagedBrokerEndpoint {
        ManagedBrokerEndpoint {
            endpoint: BrokerUrl::new("wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt")
                .expect("broker url"),
            provider: ManagedBrokerProvider::AwsIotCore,
            region: ManagedBrokerRegion::new("us-west-2").expect("region"),
            authorizer_name: ManagedBrokerAuthorizerName::new("tycode-mobile-v1")
                .expect("authorizer"),
        }
    }

    fn managed_credentials() -> ManagedBrokerCredentials {
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-tycode-grant".to_owned(),
            "signed-grant-secret".to_owned(),
        );
        ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new("grant_01J").expect("grant id"),
            client_id: ManagedBrokerClientId::new("tyde/prod/pair_01J/host/grant_01J")
                .expect("client id"),
            connect: ManagedBrokerConnectAuth {
                username: Some("x-amz-customauthorizer-name=tycode-mobile-v1".to_owned()),
                password: Some("signed-grant-secret".to_owned()),
                websocket_url: None,
                headers,
            },
            scope: ManagedBrokerCredentialScope {
                namespace: ManagedBrokerTopicNamespace::new("tyde/prod/pair_01J")
                    .expect("namespace"),
                role: ManagedBrokerRole::Host,
                publish: vec!["tyde/prod/pair_01J/rooms/+/host-to-client".to_owned()],
                subscribe: vec!["tyde/prod/pair_01J/rooms/+/client-to-host".to_owned()],
            },
            issued_at_ms: 1,
            expires_at_ms: 2,
        }
    }

    fn active_managed_pairing() -> ActiveMobilePairingCredential {
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        ActiveMobilePairingCredential {
            offer_id: MobilePairingOfferId::new("offer-1").expect("offer id"),
            broker: BrokerEndpoint {
                url: managed_broker().endpoint,
                auth: BrokerAuth::Anonymous,
            },
            room: RoomId([7_u8; 16]),
            psk: psk.clone(),
            created_at_ms: 1,
            key_fingerprint: key_fingerprint(&psk),
            managed: Some(ActiveManagedMobilePairingCredential {
                host_offer_token: "host_offer_token_secret".to_owned(),
                pairing_url: "https://tycode.dev/tyde/#tyde-pair://v2?secret-payload".to_owned(),
                broker: managed_broker(),
                host_broker_credentials: managed_credentials(),
                expires_at_ms: 2,
                handoff: Some(ManagedMobilePairingHandoff {
                    pairing_id: "pair_01J".to_owned(),
                    host_pairing_secret: "handoff_host_pairing_secret".to_owned(),
                    device_id: MobileDeviceId("device-1".to_owned()),
                    device_label: "Mike's iPhone".to_owned(),
                    device_created_at_ms: 1,
                    device_last_seen_at_ms: Some(1),
                    broker: managed_broker(),
                    host_broker_credentials: managed_credentials(),
                }),
            }),
        }
    }

    #[test]
    fn pairings_save_omits_active_pairing_and_round_trips_devices() {
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
                managed: None,
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
                managed: None,
            }],
        };

        store.save(&pairings).expect("save pairings");
        let loaded = store.get().expect("load pairings");
        assert!(loaded.active_pairing.is_none());
        assert_eq!(loaded.devices, pairings.devices);
    }

    #[test]
    fn active_managed_pairing_secrets_are_not_serialized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let store = MobilePairingsStore::load(path.clone()).expect("load store");
        let pairings = MobilePairings {
            version: MOBILE_PAIRINGS_VERSION,
            active_pairing: Some(active_managed_pairing()),
            devices: Vec::new(),
        };

        store.save(&pairings).expect("save pairings");
        let json = std::fs::read_to_string(&path).expect("read pairings file");

        assert!(!json.contains("active_pairing"));
        assert!(!json.contains("host_offer_token_secret"));
        assert!(!json.contains("secret-payload"));
        assert!(!json.contains("signed-grant-secret"));
        assert!(!json.contains("handoff_host_pairing_secret"));
        assert!(store.get().expect("load pairings").active_pairing.is_none());
    }

    #[test]
    fn durable_managed_pairing_credentials_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let store = MobilePairingsStore::load(path).expect("load store");
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        let pairings = MobilePairings {
            version: MOBILE_PAIRINGS_VERSION,
            active_pairing: None,
            devices: vec![MobilePairingRecord {
                device_id: MobileDeviceId("device-1".to_owned()),
                broker: BrokerEndpoint {
                    url: managed_broker().endpoint,
                    auth: BrokerAuth::Anonymous,
                },
                room: RoomId([8_u8; 16]),
                psk: psk.clone(),
                label: "Mike's iPhone".to_owned(),
                created_at_ms: 1,
                last_seen_at_ms: Some(1),
                state: MobileDeviceState::Paired,
                key_fingerprint: key_fingerprint(&psk),
                managed: Some(ManagedMobilePairingCredential {
                    pairing_id: "pair_01J".to_owned(),
                    host_pairing_secret: "durable_host_pairing_secret".to_owned(),
                    broker: managed_broker(),
                }),
            }],
        };

        store.save(&pairings).expect("save pairings");
        let loaded = store.get().expect("load pairings");

        assert_eq!(loaded, pairings);
        let managed = loaded.devices[0].managed.as_ref().expect("managed pairing");
        assert_eq!(managed.pairing_id, "pair_01J");
        assert_eq!(managed.host_pairing_secret, "durable_host_pairing_secret");
    }

    #[test]
    fn unknown_store_version_loads_as_repair_required() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        let mut pairings = MobilePairings {
            version: MOBILE_PAIRINGS_VERSION,
            active_pairing: Some(ActiveMobilePairingCredential {
                offer_id: MobilePairingOfferId::new("offer-1").expect("offer id"),
                broker: endpoint(),
                room: RoomId([7_u8; 16]),
                psk: psk.clone(),
                created_at_ms: 1,
                key_fingerprint: key_fingerprint(&psk),
                managed: None,
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
                key_fingerprint: "stale-fingerprint".to_owned(),
                managed: None,
            }],
        };
        pairings.version = 1;
        let json = serde_json::to_vec_pretty(&pairings).expect("serialize old pairings");
        atomic_write_owner_only(&path, &json).expect("write old pairings");

        let store = MobilePairingsStore::load(path).expect("load store handle");
        let loaded = store.get().expect("recover old pairings");

        assert_eq!(loaded.version, MOBILE_PAIRINGS_VERSION);
        assert!(loaded.active_pairing.is_none());
        assert_eq!(loaded.devices.len(), 1);
        assert_eq!(loaded.devices[0].state, MobileDeviceState::RepairRequired);
        assert!(loaded.devices[0].managed.is_none());
        assert_eq!(loaded.devices[0].key_fingerprint, key_fingerprint(&psk));
    }

    #[test]
    fn incomplete_managed_metadata_starts_empty_instead_of_failing_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let json = serde_json::json!({
            "version": MOBILE_PAIRINGS_VERSION,
            "active_pairing": null,
            "devices": [{
                "device_id": "device-1",
                "broker": endpoint(),
                "room": RoomId([8_u8; 16]),
                "psk": PreSharedKey::from_slice(&[9_u8; 32]).expect("psk"),
                "label": "Mike's iPhone",
                "created_at_ms": 1,
                "last_seen_at_ms": null,
                "state": "paired",
                "key_fingerprint": "stale-fingerprint",
                "managed": {}
            }]
        });
        let json = serde_json::to_vec_pretty(&json).expect("serialize malformed pairings");
        atomic_write_owner_only(&path, &json).expect("write malformed pairings");

        let store = MobilePairingsStore::load(path).expect("load store handle");
        let loaded = store.get().expect("malformed store fails closed");

        assert_eq!(loaded, MobilePairings::empty());
    }

    #[test]
    fn debug_redacts_mobile_pairing_secrets() {
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        let record = MobilePairingRecord {
            device_id: MobileDeviceId("device-1".to_owned()),
            broker: endpoint(),
            room: RoomId([8_u8; 16]),
            psk: psk.clone(),
            label: "Mike's iPhone".to_owned(),
            created_at_ms: 1,
            last_seen_at_ms: None,
            state: MobileDeviceState::Paired,
            key_fingerprint: key_fingerprint(&psk),
            managed: Some(ManagedMobilePairingCredential {
                pairing_id: "pair_01J".to_owned(),
                host_pairing_secret: "host_pairing_secret_raw".to_owned(),
                broker: ManagedBrokerEndpoint {
                    endpoint: BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
                    )
                    .expect("broker url"),
                    provider: protocol::ManagedBrokerProvider::AwsIotCore,
                    region: protocol::ManagedBrokerRegion::new("us-west-2").expect("region"),
                    authorizer_name: protocol::ManagedBrokerAuthorizerName::new("tycode-mobile-v1")
                        .expect("authorizer"),
                },
            }),
        };
        let debug = format!("{record:?}");

        assert!(!debug.contains("host_pairing_secret_raw"));
        assert!(!debug.contains(&psk.as_base64url_no_pad()));
        assert!(debug.contains("<redacted>"));
    }
}
