//! Browser (PWA) persistence for paired hosts and PSKs.
//!
//! Ports `mobile/src-tauri/src/paired_hosts.rs` (host records) and
//! `psk_store.rs` (secret storage) to IndexedDB. The Keychain / app-data-file
//! backends do not exist in a browser, so both live in IndexedDB via
//! [`super::idb`].
//!
//! ## PSK storage seam (later-phase hardening)
//!
//! For this phase the PSK is stored as raw 32 bytes (base64) in IndexedDB —
//! the documented fallback in `docs/plans/mobile-web-pwa.md` → "PSK storage".
//! The eventual hardening stores the long-term PSK as a **non-extractable
//! WebCrypto HKDF `CryptoKey`** so the root secret never exists as readable
//! bytes at rest. To keep that swap localized, all PSK access goes through the
//! [`PskStore`] trait; only [`IndexedDbPskStore`] (and the HKDF call sites it
//! feeds) change when the hardening lands. The host-record store is unaffected
//! because `WebPairedHostRecord` already stores no PSK material — only a key id
//! + credential fingerprint, exactly like the native `PairedHostRecord`.

use std::rc::Rc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mobile_shell_types::{
    BrokerAuthSummary, BrokerEndpointSummary, KeychainSecretId, LocalHostId, PairedHostSummary,
    RoomIdSummary,
};
use mqtt_transport::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::idb;

const HOSTS_KEY: &str = "all";

/// Browser-side mirror of the native `PairedHostRecord`. Stores no PSK material
/// (only a key id + fingerprint), matching the native record so the same
/// `paired-hosts-changed` summaries can be produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPairedHostRecord {
    pub local_host_id: LocalHostId,
    pub host_label: String,
    pub broker: BrokerEndpoint,
    pub room: RoomId,
    pub psk_keychain_key_id: KeychainSecretId,
    pub credential_fingerprint: String,
    pub auto_connect: bool,
    pub last_connected_at_ms: Option<u64>,
}

impl WebPairedHostRecord {
    pub fn summary(&self) -> PairedHostSummary {
        PairedHostSummary {
            local_host_id: self.local_host_id.clone(),
            host_label: self.host_label.clone(),
            broker: BrokerEndpointSummary {
                url: self.broker.url.clone(),
                auth: match &self.broker.auth {
                    BrokerAuth::Anonymous => BrokerAuthSummary::Anonymous,
                    BrokerAuth::UsernamePassword { username, password } => {
                        BrokerAuthSummary::UsernamePassword {
                            username: username.clone(),
                            has_password: !password.is_empty(),
                        }
                    }
                },
            },
            room: RoomIdSummary(self.room.to_string()),
            credential_fingerprint: self.credential_fingerprint.clone(),
            auto_connect: self.auto_connect,
            last_connected_at_ms: self.last_connected_at_ms,
        }
    }
}

/// Stable, traversal-safe 16-char credential fingerprint. Byte-for-byte port of
/// `paired_hosts::credential_fingerprint` (SHA-256 over broker URL + room +
/// PSK), computed at pairing time while the raw PSK bytes are still in hand.
pub fn credential_fingerprint(
    broker: &BrokerEndpoint,
    room: &RoomId,
    psk: &PreSharedKey,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(broker.url.as_str().as_bytes());
    hasher.update(room.as_base64url_no_pad().as_bytes());
    hasher.update(psk.as_bytes());
    let encoded = URL_SAFE_NO_PAD.encode(hasher.finalize());
    encoded.chars().take(16).collect()
}

// ── Host record store ────────────────────────────────────────────────────

thread_local! {
    /// Serializes every read-modify-write on the single-key host array, the way
    /// the native single-threaded `StoreActor` did. The whole record set lives
    /// under one IndexedDB key, so without this lock two mutators can each
    /// `list()` the array, yield at an `.await`, then write back over each other:
    /// a lost `last_connected_at_ms` update, or — worse — a `set_last_connected`
    /// that read before a concurrent `forget` committed and rewrites the array
    /// *after*, resurrecting the forgotten host pointed at a deleted PSK. Holding
    /// this guard across each mutator's read→write makes those sequences atomic.
    static HOST_WRITE_LOCK: Rc<tokio::sync::Mutex<()>> = Rc::new(tokio::sync::Mutex::new(()));
}

pub struct IndexedDbHostStore;

impl IndexedDbHostStore {
    pub async fn list(&self) -> Result<Vec<WebPairedHostRecord>, String> {
        match idb::get(idb::STORE_HOSTS, HOSTS_KEY).await? {
            Some(json) => decode_records(&json),
            None => Ok(Vec::new()),
        }
    }

    pub async fn list_summaries(&self) -> Result<Vec<PairedHostSummary>, String> {
        Ok(self
            .list()
            .await?
            .iter()
            .map(WebPairedHostRecord::summary)
            .collect())
    }

    pub async fn get(
        &self,
        local_host_id: &LocalHostId,
    ) -> Result<Option<WebPairedHostRecord>, String> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|record| record.local_host_id == *local_host_id))
    }

    pub async fn insert(&self, record: WebPairedHostRecord) -> Result<(), String> {
        let lock = HOST_WRITE_LOCK.with(Rc::clone);
        let _guard = lock.lock().await;
        let mut records = self.list().await?;
        if records
            .iter()
            .any(|existing| existing.local_host_id == record.local_host_id)
        {
            return Err(format!(
                "paired host {} already exists",
                record.local_host_id
            ));
        }
        records.push(record);
        self.save(&records).await
    }

    pub async fn remove(
        &self,
        local_host_id: &LocalHostId,
    ) -> Result<Option<WebPairedHostRecord>, String> {
        let lock = HOST_WRITE_LOCK.with(Rc::clone);
        let _guard = lock.lock().await;
        let mut records = self.list().await?;
        let Some(index) = records
            .iter()
            .position(|record| record.local_host_id == *local_host_id)
        else {
            return Ok(None);
        };
        let removed = records.remove(index);
        self.save(&records).await?;
        Ok(Some(removed))
    }

    pub async fn set_auto_connect(
        &self,
        local_host_id: &LocalHostId,
        auto_connect: bool,
    ) -> Result<(), String> {
        let lock = HOST_WRITE_LOCK.with(Rc::clone);
        let _guard = lock.lock().await;
        let mut records = self.list().await?;
        let Some(record) = records
            .iter_mut()
            .find(|record| record.local_host_id == *local_host_id)
        else {
            return Err(format!("paired host {local_host_id} was not found"));
        };
        record.auto_connect = auto_connect;
        self.save(&records).await
    }

    pub async fn set_last_connected_at_ms(
        &self,
        local_host_id: &LocalHostId,
        last_connected_at_ms: Option<u64>,
    ) -> Result<(), String> {
        let lock = HOST_WRITE_LOCK.with(Rc::clone);
        let _guard = lock.lock().await;
        let mut records = self.list().await?;
        let Some(record) = records
            .iter_mut()
            .find(|record| record.local_host_id == *local_host_id)
        else {
            return Err(format!("paired host {local_host_id} was not found"));
        };
        record.last_connected_at_ms = last_connected_at_ms;
        self.save(&records).await
    }

    async fn save(&self, records: &[WebPairedHostRecord]) -> Result<(), String> {
        let json = serde_json::to_string(records)
            .map_err(|error| format!("failed to serialize paired hosts: {error}"))?;
        idb::put(idb::STORE_HOSTS, HOSTS_KEY, &json).await
    }
}

fn decode_records(json: &str) -> Result<Vec<WebPairedHostRecord>, String> {
    serde_json::from_str(json).map_err(|error| format!("failed to parse paired hosts: {error}"))
}

// ── PSK store (seam for later WebCrypto hardening) ───────────────────────

/// Storage seam for the long-term PSK. See the module docs: the only place
/// that changes when the non-extractable `CryptoKey` hardening lands.
#[allow(async_fn_in_trait)] // Single wasm impl, only ever used statically (never as `dyn`).
pub trait PskStore {
    async fn store(&self, psk: &PreSharedKey) -> Result<KeychainSecretId, String>;
    async fn load(&self, key_id: &KeychainSecretId) -> Result<PreSharedKey, String>;
    async fn delete(&self, key_id: &KeychainSecretId) -> Result<(), String>;
}

/// Raw-bytes IndexedDB PSK store (documented fallback). TODO(mobile-web-pwa,
/// "PSK storage"): replace with a non-extractable WebCrypto HKDF `CryptoKey`
/// imported at pairing time and used only via `crypto.subtle.deriveBits`, so
/// the root secret is never readable at rest. The async signatures here already
/// match what that swap needs.
pub struct IndexedDbPskStore;

impl PskStore for IndexedDbPskStore {
    async fn store(&self, psk: &PreSharedKey) -> Result<KeychainSecretId, String> {
        let key_id = KeychainSecretId(format!("tyde-web-psk-{}", uuid::Uuid::new_v4()));
        let encoded = URL_SAFE_NO_PAD.encode(psk.as_bytes());
        idb::put(idb::STORE_PSK, &key_id.0, &encoded).await?;
        Ok(key_id)
    }

    async fn load(&self, key_id: &KeychainSecretId) -> Result<PreSharedKey, String> {
        let encoded = idb::get(idb::STORE_PSK, &key_id.0)
            .await?
            .ok_or_else(|| format!("no PSK stored for key {key_id}"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded.trim().as_bytes())
            .map_err(|error| format!("stored PSK for {key_id} is not valid base64: {error}"))?;
        PreSharedKey::from_slice(&bytes)
            .map_err(|error| format!("stored PSK for {key_id} is invalid: {error}"))
    }

    async fn delete(&self, key_id: &KeychainSecretId) -> Result<(), String> {
        idb::delete(idb::STORE_PSK, &key_id.0).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker() -> BrokerEndpoint {
        BrokerEndpoint {
            url: protocol::BrokerUrl::new("wss://broker.emqx.io:8084/mqtt").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        }
    }

    fn sample_record() -> WebPairedHostRecord {
        let broker = broker();
        let room = RoomId([7_u8; 16]);
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        WebPairedHostRecord {
            local_host_id: LocalHostId("host-one".to_owned()),
            host_label: "Host One".to_owned(),
            broker: broker.clone(),
            room,
            psk_keychain_key_id: KeychainSecretId("psk-key".to_owned()),
            credential_fingerprint: credential_fingerprint(&broker, &room, &psk),
            auto_connect: true,
            last_connected_at_ms: Some(42),
        }
    }

    #[test]
    fn record_json_round_trips_and_omits_psk_bytes() {
        let record = sample_record();
        let json = serde_json::to_string(std::slice::from_ref(&record)).expect("serialize");
        let psk_b64 = PreSharedKey::from_slice(&[9_u8; 32])
            .expect("psk")
            .as_base64url_no_pad();
        assert!(
            !json.contains(&psk_b64),
            "host record JSON must not contain raw PSK bytes: {json}"
        );
        assert!(json.contains("pskKeychainKeyId"));
        let decoded = decode_records(&json).expect("decode");
        assert_eq!(decoded, vec![record]);
    }

    #[test]
    fn fingerprint_matches_native_shape() {
        let fingerprint = credential_fingerprint(
            &broker(),
            &RoomId([1_u8; 16]),
            &PreSharedKey::from_slice(&[2_u8; 32]).expect("psk"),
        );
        assert_eq!(fingerprint.len(), 16);
        assert!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn summary_redacts_broker_password() {
        let mut record = sample_record();
        record.broker = BrokerEndpoint {
            url: record.broker.url.clone(),
            auth: BrokerAuth::UsernamePassword {
                username: "mobile".to_owned(),
                password: "super-secret".to_owned(),
            },
        };
        let summary = record.summary();
        assert_eq!(
            summary.broker.auth,
            BrokerAuthSummary::UsernamePassword {
                username: "mobile".to_owned(),
                has_password: true,
            }
        );
        let encoded = serde_json::to_string(&summary).expect("summary json");
        assert!(!encoded.contains("super-secret"));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use mqtt_transport::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
    use wasm_bindgen_test::*;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn unique_record(tag: &str) -> WebPairedHostRecord {
        let broker = BrokerEndpoint {
            url: protocol::BrokerUrl::new("wss://broker.emqx.io:8084/mqtt").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        };
        let room = RoomId([5_u8; 16]);
        let psk = PreSharedKey::from_slice(&[6_u8; 32]).expect("psk");
        WebPairedHostRecord {
            local_host_id: LocalHostId(format!("host-{tag}-{}", uuid::Uuid::new_v4())),
            host_label: "Round Trip".to_owned(),
            broker: broker.clone(),
            room,
            psk_keychain_key_id: KeychainSecretId(format!("psk-{tag}")),
            credential_fingerprint: credential_fingerprint(&broker, &room, &psk),
            auto_connect: true,
            last_connected_at_ms: None,
        }
    }

    #[wasm_bindgen_test]
    async fn host_record_round_trips_through_indexeddb() {
        let store = IndexedDbHostStore;
        let record = unique_record("hosts");
        let id = record.local_host_id.clone();

        store.insert(record.clone()).await.expect("insert");
        let fetched = store.get(&id).await.expect("get").expect("present");
        assert_eq!(fetched, record);

        store
            .set_auto_connect(&id, false)
            .await
            .expect("set auto connect");
        let updated = store.get(&id).await.expect("get").expect("present");
        assert!(!updated.auto_connect);

        let removed = store.remove(&id).await.expect("remove");
        assert_eq!(removed.map(|r| r.local_host_id), Some(id.clone()));
        assert!(store.get(&id).await.expect("get").is_none());
    }

    #[wasm_bindgen_test]
    async fn forget_interleaved_with_set_last_connected_does_not_resurrect() {
        let store = IndexedDbHostStore;
        let psk_store = IndexedDbPskStore;
        let psk = PreSharedKey::from_slice(&[8_u8; 32]).expect("psk");

        // Pair: store the PSK, then a record that references it.
        let key_id = psk_store.store(&psk).await.expect("store psk");
        let mut record = unique_record("race");
        record.psk_keychain_key_id = key_id.clone();
        let id = record.local_host_id.clone();
        store.insert(record).await.expect("insert");

        // Interleave a last-connected write with a forget (remove record + delete
        // PSK) on the same cooperative task. With the write lock these serialize;
        // without it the set-last-connected RMW could resurrect the removed host.
        let set_fut = store.set_last_connected_at_ms(&id, Some(987_654));
        let forget_fut = async {
            let _ = store.remove(&id).await;
            let _ = psk_store.delete(&key_id).await;
        };
        let (_set_result, ()) = tokio::join!(set_fut, forget_fut);

        assert!(
            store.get(&id).await.expect("get").is_none(),
            "forgotten host must not be resurrected by a concurrent write"
        );
        assert!(
            psk_store.load(&key_id).await.is_err(),
            "the removed host's PSK must not dangle"
        );
    }

    #[wasm_bindgen_test]
    async fn psk_round_trips_through_indexeddb() {
        let store = IndexedDbPskStore;
        let psk = PreSharedKey::from_slice(&[7_u8; 32]).expect("psk");

        let key_id = store.store(&psk).await.expect("store psk");
        let loaded = store.load(&key_id).await.expect("load psk");
        assert_eq!(loaded, psk);

        store.delete(&key_id).await.expect("delete psk");
        assert!(
            store.load(&key_id).await.is_err(),
            "deleted psk must be gone"
        );
    }
}
