//! Browser (PWA) bridge backend.
//!
//! Direct-to-wasm equivalents of the Tauri `mobile-shell` commands/events:
//! transport + connection manager ([`connection`]), IndexedDB persistence
//! ([`store`]), QR scanning ([`qr`]), and an in-process event hub ([`events`]).
//! Selected at runtime by [`super`] when `window.__TAURI__` is absent.

mod connection;
mod events;
mod idb;
mod qr;
mod store;

use host_config::HostLineEvent;
use mobile_shell_types::{
    KnownConnectionInstance, LocalHostId, MobilePairingPreview, PairedHostConnectionStatusEvent,
    PairedHostSummary,
};
use mqtt_transport::{MOBILE_QR_VERSION, MobilePairingQrPayload};
use protocol::PROTOCOL_VERSION;

use super::UnlistenHandle;
use store::{IndexedDbHostStore, IndexedDbPskStore, PskStore, WebPairedHostRecord};

pub use qr::{ensure_camera_permission, scan_qr};

// ── Paired-host queries ───────────────────────────────────────────────────

pub async fn list_paired_hosts() -> Result<Vec<PairedHostSummary>, String> {
    IndexedDbHostStore.list_summaries().await
}

pub async fn list_paired_host_connection_statuses()
-> Result<Vec<PairedHostConnectionStatusEvent>, String> {
    Ok(connection::manager().connection_statuses())
}

/// The browser delivers host lines straight to the live listener, so there is
/// never a detached backlog to drain (see [`connection`] module docs).
pub async fn list_pending_host_lines() -> Result<Vec<HostLineEvent>, String> {
    Ok(Vec::new())
}

// ── Pairing ───────────────────────────────────────────────────────────────

/// sessionStorage key under which the web loader (`web/loader/loader.js`)
/// stashes the full raw `tyde-pair://…` pairing URI before it boots this
/// bundle, so first-time pairing can complete without a second scan. MUST stay
/// in sync with `PAIR_URI_KEY` in the loader.
const PENDING_PAIRING_URI_KEY: &str = "tyde.pair.uri";

/// Reads and CLEARS the pending pairing URI the loader stashed (if any).
///
/// The URI is returned raw and unparsed — callers run the authoritative
/// [`parse_and_validate`] / [`preview_pairing_uri`] themselves; the loader is
/// trusted only to have routed us here, not to have validated the payload. The
/// key is always cleared so a stale URI cannot replay on a later reload.
pub fn take_pending_pairing_uri() -> Option<String> {
    let storage = web_sys::window()?.session_storage().ok()??;
    let value = storage.get_item(PENDING_PAIRING_URI_KEY).ok()??;
    // Clear regardless of contents so a malformed/forged stash cannot persist.
    let _ = storage.remove_item(PENDING_PAIRING_URI_KEY);
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

pub async fn preview_pairing_uri(qr_uri: &str) -> Result<MobilePairingPreview, String> {
    let payload = parse_and_validate(qr_uri)?;
    Ok(MobilePairingPreview {
        host_label: normalize_host_label(payload.host_label)?,
        broker_url: payload.broker.url,
    })
}

pub async fn start_pairing(qr_uri: &str) -> Result<(), String> {
    let payload = parse_and_validate(qr_uri)?;

    let psk_store = IndexedDbPskStore;
    let key_id = psk_store.store(&payload.psk).await?;
    let fingerprint = store::credential_fingerprint(&payload.broker, &payload.room, &payload.psk);
    let record = WebPairedHostRecord {
        local_host_id: LocalHostId(uuid::Uuid::new_v4().to_string()),
        host_label: normalize_host_label(payload.host_label.clone())?,
        broker: payload.broker.clone(),
        room: payload.room,
        psk_keychain_key_id: key_id.clone(),
        credential_fingerprint: fingerprint,
        auto_connect: true,
        last_connected_at_ms: None,
    };
    let local_host_id = record.local_host_id.clone();

    let host_store = IndexedDbHostStore;
    if let Err(store_error) = host_store.insert(record).await {
        let _ = psk_store.delete(&key_id).await;
        return Err(store_error);
    }

    if let Err(connect_error) = connection::manager().connect(local_host_id.clone()).await {
        let _ = host_store.remove(&local_host_id).await;
        let _ = psk_store.delete(&key_id).await;
        return Err(connect_error);
    }

    emit_paired_hosts_changed().await;
    Ok(())
}

// ── Connection control ────────────────────────────────────────────────────

pub async fn connect_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    connection::manager().connect(local_host_id.clone()).await
}

pub async fn disconnect_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    connection::manager().disconnect(local_host_id.clone())
}

pub async fn forget_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    let host_store = IndexedDbHostStore;
    let record = host_store
        .get(local_host_id)
        .await?
        .ok_or_else(|| format!("paired host {local_host_id} was not found"))?;
    // Best-effort disconnect (ignore "no active connection").
    let _ = connection::manager().disconnect(local_host_id.clone());
    IndexedDbPskStore
        .delete(&record.psk_keychain_key_id)
        .await?;
    host_store.remove(local_host_id).await?;
    emit_paired_hosts_changed().await;
    Ok(())
}

pub async fn set_paired_host_auto_connect(
    local_host_id: &LocalHostId,
    auto_connect: bool,
) -> Result<(), String> {
    IndexedDbHostStore
        .set_auto_connect(local_host_id, auto_connect)
        .await?;
    emit_paired_hosts_changed().await;
    Ok(())
}

pub async fn send_host_line(local_host_id: &LocalHostId, line: &str) -> Result<(), String> {
    connection::manager()
        .send_line(local_host_id.clone(), line.to_owned())
        .await
}

/// No-op: the browser path never assigns delivery ids, so there is nothing to
/// acknowledge (see [`connection`] module docs).
pub async fn ack_host_line(_local_host_id: &LocalHostId, _delivery_id: u64) -> Result<(), String> {
    Ok(())
}

/// Replays current statuses and auto-connects any `auto_connect` host that is
/// not already live — the browser equivalent of the Tauri boot flow's
/// auto-connect, which has no separate process to do it here.
pub async fn frontend_attached(
    _known_connection_instance_ids: &[KnownConnectionInstance],
) -> Result<(), String> {
    let manager = connection::manager();
    manager.frontend_attached();
    let records = IndexedDbHostStore.list().await?;
    for record in records.into_iter().filter(|record| record.auto_connect) {
        if let Err(error) = manager.connect(record.local_host_id.clone()).await {
            log::error!("auto-connect for {} failed: {error}", record.local_host_id);
        }
    }
    Ok(())
}

pub async fn wasm_log(level: &str, message: &str) {
    match level {
        "error" => log::error!("[web] {message}"),
        "warn" => log::warn!("[web] {message}"),
        "trace" => log::trace!("[web] {message}"),
        _ => log::info!("[web] {message}"),
    }
}

// ── Event listeners ───────────────────────────────────────────────────────

pub async fn listen_host_line(
    callback: impl Fn(HostLineEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(events::on_host_line(callback)))
}

pub async fn listen_host_disconnected(
    callback: impl Fn(host_config::HostDisconnectedEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(events::on_host_disconnected(
        callback,
    )))
}

pub async fn listen_host_error(
    callback: impl Fn(host_config::HostErrorEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(events::on_host_error(
        callback,
    )))
}

pub async fn listen_paired_hosts_changed(
    callback: impl Fn(mobile_shell_types::PairedHostsChangedEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(
        events::on_paired_hosts_changed(callback),
    ))
}

pub async fn listen_paired_host_connection_status(
    callback: impl Fn(PairedHostConnectionStatusEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(events::on_connection_status(
        callback,
    )))
}

pub async fn listen_mobile_shell_error(
    callback: impl Fn(mobile_shell_types::MobileShellError) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(events::on_shell_error(
        callback,
    )))
}

// ── Helpers (ported from mobile/src-tauri/src/lib.rs) ──────────────────────

async fn emit_paired_hosts_changed() {
    match IndexedDbHostStore.list_summaries().await {
        Ok(hosts) => {
            events::emit_paired_hosts_changed(mobile_shell_types::PairedHostsChangedEvent { hosts })
        }
        Err(error) => log::warn!("failed to emit paired hosts changed: {error}"),
    }
}

fn parse_and_validate(qr_uri: &str) -> Result<MobilePairingQrPayload, String> {
    let payload = MobilePairingQrPayload::from_uri(qr_uri)
        .map_err(|error| format!("invalid mobile pairing URI: {error}"))?;
    if payload.v != MOBILE_QR_VERSION {
        return Err(format!(
            "unsupported mobile pairing QR version {}, expected {}",
            payload.v, MOBILE_QR_VERSION
        ));
    }
    if payload.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported Tyde protocol version {}, expected {}",
            payload.protocol_version, PROTOCOL_VERSION
        ));
    }
    let _ = normalize_host_label(payload.host_label.clone())?;
    Ok(payload)
}

fn normalize_host_label(host_label: String) -> Result<String, String> {
    let trimmed = host_label.trim().to_owned();
    if trimmed.is_empty() {
        return Err("mobile pairing QR host_label must not be empty".to_owned());
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use mqtt_transport::{PreSharedKey, RoomId, default_mobile_broker_endpoint};

    use super::*;

    fn valid_uri() -> String {
        MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([3_u8; 16]),
            PreSharedKey::from_slice(&[4_u8; 32]).expect("psk"),
            "Living Room".to_owned(),
        )
        .to_uri()
        .expect("encode pairing uri")
    }

    #[test]
    fn normalize_host_label_trims_and_rejects_empty() {
        assert_eq!(normalize_host_label("  Host  ".to_owned()).unwrap(), "Host");
        assert!(normalize_host_label("   ".to_owned()).is_err());
    }

    #[test]
    fn parse_and_validate_round_trips_a_valid_uri() {
        let payload = parse_and_validate(&valid_uri()).expect("valid pairing uri");
        assert_eq!(payload.host_label, "Living Room");
        assert_eq!(payload.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn parse_and_validate_rejects_non_pairing_uri() {
        let error = parse_and_validate("https://example.com/not-a-pairing-uri")
            .expect_err("must reject non tyde-pair uris");
        assert!(error.contains("invalid mobile pairing URI"), "{error}");
    }

    #[test]
    fn parse_and_validate_rejects_protocol_mismatch() {
        let mut payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION + 1,
            default_mobile_broker_endpoint(),
            RoomId([3_u8; 16]),
            PreSharedKey::from_slice(&[4_u8; 32]).expect("psk"),
            "Host".to_owned(),
        );
        payload.protocol_version = PROTOCOL_VERSION + 1;
        let uri = payload.to_uri().expect("encode");
        let error = parse_and_validate(&uri).expect_err("protocol mismatch must be rejected");
        assert!(error.contains("protocol version"), "{error}");
    }
}
