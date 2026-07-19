//! Browser (PWA) bridge backend.
//!
//! Direct-to-wasm host I/O: transport + connection manager ([`connection`]),
//! IndexedDB persistence ([`store`]), QR scanning ([`qr`]), and an in-process
//! event hub ([`events`]).

mod connection;
mod events;
mod idb;
mod qr;
mod service;
mod store;

use host_config::HostLineEvent;
use mobile_shell_types::{
    KnownConnectionInstance, LocalHostId, PairedHostConnectionStatusEvent, PairedHostSummary,
};
use mqtt_transport::{
    MOBILE_QR_VERSION, MobilePairingQrOffer, MobilePairingQrPayload, parse_mobile_pairing_qr_offer,
};
use protocol::PROTOCOL_VERSION;

use crate::state::PairingOffer;

use super::{
    Accepted, ConnectionInvalidation, InvalidationRejected, SendRejected,
    SubmissionTransportOutcomeEvent, UnlistenHandle,
};
use store::{IndexedDbHostStore, IndexedDbPskStore, PskStore, WebPairedHostRecord};

pub use qr::{ensure_camera_permission, scan_qr};
pub use service::{
    AuthProvider, RedeemOutcome, authenticate as authenticate_managed,
    complete_boot_auth_callback as complete_boot_managed_auth_callback,
    probe_auth as probe_managed_auth,
};

#[cfg(all(test, target_arch = "wasm32"))]
pub use connection::{
    TestSendGuard, test_capture_sends, test_clean_sends, test_defer_sends, test_reject_sends,
    test_resolve_next_send, test_send_attempts, test_sent_lines,
};

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
/// [`classify_pairing_offer`] themselves; the loader is trusted only to have
/// routed us here, not to have validated the payload. The key is always cleared
/// so a stale URI cannot replay on a later reload.
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

/// Classifies a scanned/pasted pairing URI into the typed [`PairingOffer`] the
/// pairing flow renders from. Managed (`tyde-pair://v2`) offers drive the
/// `tycode.dev` auth + redeem sequence; legacy (`tyde-pair://v1`) public-broker
/// offers fail closed to a repair-required screen — never a silent legacy
/// connect (locked decision #8). A protocol-version mismatch triggers the same
/// loader self-heal reboot the legacy path uses, then returns an error so this
/// bundle never proceeds.
pub async fn classify_pairing_offer(qr_uri: &str) -> Result<PairingOffer, String> {
    let offer = parse_mobile_pairing_qr_offer(qr_uri)
        .map_err(|error| format!("invalid mobile pairing URI: {error}"))?;
    match offer {
        MobilePairingQrOffer::ManagedService(payload) => {
            if payload.protocol_version != PROTOCOL_VERSION {
                request_loader_repair(qr_uri);
                return Err(format!(
                    "unsupported Tyde protocol version {}, expected {}",
                    payload.protocol_version, PROTOCOL_VERSION
                ));
            }
            let host_label = normalize_host_label(payload.host_label.clone())?;
            Ok(PairingOffer::ManagedService { host_label })
        }
        MobilePairingQrOffer::LegacyPublicBrokerRepairRequired(_) => {
            Ok(PairingOffer::RepairRequired {
                message: LEGACY_QR_REPAIR_MESSAGE.to_owned(),
            })
        }
    }
}

/// Redeems a managed offer with `tycode.dev` and connects to the managed broker.
/// Thin re-export of the [`service`] seam so the pairing flow calls it through
/// the `bridge` façade like every other host action.
pub async fn redeem_managed_and_connect(qr_uri: &str) -> Result<(), RedeemOutcome> {
    service::redeem_and_connect(qr_uri).await
}

/// User-facing copy for a legacy public-broker QR that fails closed. Kept in one
/// place so the scan-time and stored-record repair surfaces stay consistent.
const LEGACY_QR_REPAIR_MESSAGE: &str = "This is an older Tyde pairing code that used the shared public broker, which is no longer supported. Open Tyde on your computer, open the Mobile tab in Settings (Settings → Mobile), turn on mobile access again, and scan the new QR code to re-pair.";

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
        managed: None,
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
    // Drop the in-memory managed broker grant so a forgotten host can't reuse it.
    service::clear_cached_credentials(local_host_id);

    // Attempt every deletion so a single failure can't strand the rest, and
    // report all failures explicitly rather than silently ignoring them
    // (finding #8). The PSK and the managed device pairing secret both live in
    // the secret store; the record itself lives in the host store.
    let mut failures: Vec<String> = Vec::new();
    if let Err(error) = IndexedDbPskStore.delete(&record.psk_keychain_key_id).await {
        failures.push(format!("PSK ({}): {error}", record.psk_keychain_key_id));
    }
    if let Some(managed) = record.managed.as_ref()
        && let Err(error) = store::delete_device_secret(&managed.device_secret_key_id).await
    {
        failures.push(format!(
            "device secret ({}): {error}",
            managed.device_secret_key_id
        ));
    }
    if let Err(error) = host_store.remove(local_host_id).await {
        failures.push(format!("host record: {error}"));
    }
    emit_paired_hosts_changed().await;

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "paired host {local_host_id} was only partially forgotten; retry to finish cleanup ({})",
            failures.join("; ")
        ))
    }
}

/// Web-only: start the Tyggs sign-in through `tycode.dev`. When `resume_qr_uri`
/// is `Some` (mid-pairing), the scanned URI is stashed so the flow resumes after
/// the redirect; when `None` (reconnecting an already-paired host), only the
/// sign-in redirect happens. The selected provider is validated against the
/// loader config before the page navigates to the `tycode.dev`-hosted OAuth
/// start URL; `tycode.dev` completes the Tyggs dance and sets the session cookie
/// — no Tyggs secret ever reaches JS.
pub fn tyggs_auth_providers() -> Result<Vec<AuthProvider>, String> {
    service::auth_providers()
}

pub fn begin_tyggs_sign_in(
    provider: AuthProvider,
    resume_qr_uri: Option<&str>,
) -> Result<(), String> {
    let url = service::tyggs_sign_in_url(provider)?;
    if let Some(qr_uri) = resume_qr_uri {
        stash_pending_pairing_uri(qr_uri);
    }
    let window = web_sys::window().ok_or("no window to start sign-in")?;
    window
        .location()
        .set_href(&url)
        .map_err(|error| format!("failed to start Tyggs sign-in: {}", js_error_string(&error)))
}

/// Stashes the raw pairing URI in the same sessionStorage slot the loader uses,
/// so [`take_pending_pairing_uri`] resumes pairing when the OAuth redirect
/// returns to this bundle.
fn stash_pending_pairing_uri(qr_uri: &str) {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let _ = storage.set_item(PENDING_PAIRING_URI_KEY, qr_uri);
}

fn js_error_string(value: &wasm_bindgen::JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
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

pub async fn send_host_line(
    local_host_id: &LocalHostId,
    line: &str,
) -> Result<Accepted, SendRejected> {
    #[cfg(all(feature = "ui-fixtures", debug_assertions))]
    if crate::fixtures::is_requested() {
        return Ok(crate::fixtures::capture_send(line));
    }

    connection::manager()
        .send_line(local_host_id.clone(), line.to_owned())
        .await
}

pub fn invalidate_host_connection(
    local_host_id: &LocalHostId,
    reason: ConnectionInvalidation,
) -> Result<(), InvalidationRejected> {
    connection::manager().invalidate(local_host_id, reason)
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

pub async fn listen_submission_transport_outcome(
    callback: impl Fn(SubmissionTransportOutcomeEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    Ok(UnlistenHandle::from_cleanup(
        connection::on_submission_transport_outcome(callback),
    ))
}

// ── Pairing helpers ────────────────────────────────────────────────────────

async fn emit_paired_hosts_changed() {
    match IndexedDbHostStore.list_summaries().await {
        Ok(hosts) => {
            events::emit_paired_hosts_changed(mobile_shell_types::PairedHostsChangedEvent { hosts })
        }
        Err(error) => log::warn!("failed to emit paired hosts changed: {error}"),
    }
}

fn parse_and_validate(qr_uri: &str) -> Result<MobilePairingQrPayload, String> {
    let payload = MobilePairingQrPayload::from_any(qr_uri)
        .map_err(|error| format!("invalid mobile pairing URI: {error}"))?;
    if payload.v != MOBILE_QR_VERSION {
        return Err(format!(
            "unsupported mobile pairing QR version {}, expected {}",
            payload.v, MOBILE_QR_VERSION
        ));
    }
    if payload.protocol_version != PROTOCOL_VERSION {
        // Web self-heal: this PWA bundle's compiled protocol no longer matches
        // the host. Ask the loader to forget the stale bundle and reboot into the
        // version-matched one, carrying the raw pairing URI so it can re-pair
        // without a second scan. The strict check is preserved — we STILL return
        // Err so this bundle never proceeds; the matching bundle the loader boots
        // re-runs this exact validation authoritatively.
        //
        // This self-heal exists only because THIS bundle ships the dispatch
        // below. A bundle built before this change (e.g. an already-running older
        // beta) has no such code, so it cannot retroactively self-heal — it just
        // surfaces the returned error. The loader bounds repeated reboots so a
        // misconfigured release can't spin forever.
        request_loader_repair(qr_uri);
        return Err(format!(
            "unsupported Tyde protocol version {}, expected {}",
            payload.protocol_version, PROTOCOL_VERSION
        ));
    }
    let _ = normalize_host_label(payload.host_label.clone())?;
    Ok(payload)
}

/// Dispatch the PWA loader's `tyde:repair-needed` event with the raw pairing URI
/// so the loader forgets the stale bundle and reboots into the version-matched
/// one (see `web/loader/loader.js` `onRepairNeeded`). Best-effort: any failure
/// (no window, CustomEvent unavailable) just leaves the returned `Err` to be
/// shown.
///
/// wasm-only: `web_sys::window()` is a wasm-bindgen import that panics on a
/// native target, and `parse_and_validate` runs in native unit tests, so the
/// non-wasm build gets a no-op stub.
#[cfg(target_arch = "wasm32")]
fn request_loader_repair(qr_uri: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let init = web_sys::CustomEventInit::new();
    init.set_detail(&wasm_bindgen::JsValue::from_str(qr_uri));
    if let Ok(event) = web_sys::CustomEvent::new_with_event_init_dict("tyde:repair-needed", &init) {
        let _ = window.dispatch_event(&event);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn request_loader_repair(_qr_uri: &str) {}

/// Dispatch the PWA loader's `tyde:repair-version` event carrying the host's
/// validated release version so the loader forgets the stale remembered bundle
/// and reboots into the version-matched one (see `web/loader/loader.js`
/// `onRepairVersion`). Unlike [`request_loader_repair`], this carries no
/// pairing URI — the reconnect path already has the paired host stored in
/// IndexedDB, so the rebooted bundle restores it and reconnects without a
/// re-scan. Best-effort: any failure (no window, CustomEvent unavailable)
/// leaves the sticky `UpdateRequired` error as the visible surface.
///
/// wasm-only for the same reason as [`request_loader_repair`]: `web_sys::window`
/// is a wasm-bindgen import, and the native build (unit tests) gets a no-op.
#[cfg(target_arch = "wasm32")]
pub fn request_loader_repair_version(release_version: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let init = web_sys::CustomEventInit::new();
    init.set_detail(&wasm_bindgen::JsValue::from_str(release_version));
    if let Ok(event) = web_sys::CustomEvent::new_with_event_init_dict("tyde:repair-version", &init)
    {
        let _ = window.dispatch_event(&event);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn request_loader_repair_version(_release_version: &str) {}

fn normalize_host_label(host_label: String) -> Result<String, String> {
    let trimmed = host_label.trim().to_owned();
    if trimmed.is_empty() {
        return Err("mobile pairing QR host_label must not be empty".to_owned());
    }
    Ok(trimmed)
}

/// Shared managed-offer fixtures for the web-bridge wasm tests (this module and
/// [`service`]). Kept here so both test suites build identical managed URIs.
#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod tests_support {
    use mqtt_transport::{
        ManagedMobilePairingQrPayload, ManagedMobilePairingQrPayloadParams, MobilePairingQrPayload,
        PreSharedKey, RoomId, default_mobile_broker_endpoint,
    };
    use protocol::{
        BrokerUrl, ManagedBrokerAuthorizerName, ManagedBrokerEndpoint, ManagedBrokerProvider,
        ManagedBrokerRegion, MobilePairingOfferId, PROTOCOL_VERSION, TydeReleaseVersion,
    };

    pub fn sample_managed_broker() -> ManagedBrokerEndpoint {
        ManagedBrokerEndpoint {
            endpoint: BrokerUrl::new("wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt")
                .expect("managed broker url"),
            provider: ManagedBrokerProvider::AwsIotCore,
            region: ManagedBrokerRegion::new("us-west-2").expect("region"),
            authorizer_name: ManagedBrokerAuthorizerName::new("tycode-mobile-v1")
                .expect("authorizer"),
        }
    }

    /// A managed v2 offer with deterministic room + PSK, so tests can assert the
    /// scanned rendezvous material is preserved through redeem/persistence.
    pub fn sample_managed_payload() -> ManagedMobilePairingQrPayload {
        ManagedMobilePairingQrPayload::new_with_rendezvous(ManagedMobilePairingQrPayloadParams {
            protocol_version: PROTOCOL_VERSION,
            release_version: TydeReleaseVersion::parse("0.8.19").expect("release version"),
            offer_id: MobilePairingOfferId::new("offer_01J").expect("offer id"),
            offer_secret: "offer_secret_from_qr".to_owned(),
            broker: sample_managed_broker(),
            room: RoomId([5_u8; 16]),
            psk: PreSharedKey::from_slice(&[6_u8; 32]).expect("psk"),
            host_label: "Living Room".to_owned(),
            expires_at_ms: 4_102_444_800_000,
        })
    }

    pub fn sample_managed_uri() -> String {
        sample_managed_payload()
            .to_uri()
            .expect("encode managed pairing uri")
    }

    /// A managed URI whose embedded protocol version deliberately mismatches this
    /// build, to exercise the loader self-heal path.
    pub fn mismatched_protocol_managed_uri() -> String {
        let mut payload = ManagedMobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            TydeReleaseVersion::parse("0.8.19").expect("release version"),
            MobilePairingOfferId::new("offer_01J").expect("offer id"),
            "offer_secret_from_qr".to_owned(),
            sample_managed_broker(),
            "Living Room".to_owned(),
            4_102_444_800_000,
        );
        payload.protocol_version = PROTOCOL_VERSION + 1;
        payload.to_uri().expect("encode managed pairing uri")
    }

    pub fn legacy_public_broker_uri() -> String {
        MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([3_u8; 16]),
            PreSharedKey::from_slice(&[4_u8; 32]).expect("psk"),
            "Living Room".to_owned(),
        )
        .to_uri()
        .expect("encode legacy pairing uri")
    }
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
    fn parse_and_validate_accepts_https_fragment_pairing_uri() {
        let uri = valid_uri();
        let wrapped = format!("https://tycode.dev/tyde/#{uri}");
        assert!(
            MobilePairingQrPayload::from_uri(&wrapped).is_err(),
            "the raw URI parser must reject the HTTPS wrapper"
        );
        let payload = parse_and_validate(&wrapped).expect("https fragment pairing uri");
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use mqtt_transport::{PreSharedKey, RoomId, default_mobile_broker_endpoint};
    use wasm_bindgen_test::*;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

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

    #[wasm_bindgen_test]
    fn parse_and_validate_accepts_https_fragment_qr_value() {
        let uri = valid_uri();
        let wrapped = format!("https://tycode.dev/tyde/#{uri}");
        assert!(MobilePairingQrPayload::from_uri(&wrapped).is_err());
        let payload = parse_and_validate(&wrapped).expect("https fragment pairing uri");
        assert_eq!(payload.host_label, "Living Room");
        assert_eq!(payload.protocol_version, PROTOCOL_VERSION);
    }

    fn mismatched_protocol_uri() -> String {
        MobilePairingQrPayload::new(
            PROTOCOL_VERSION + 1,
            default_mobile_broker_endpoint(),
            RoomId([3_u8; 16]),
            PreSharedKey::from_slice(&[4_u8; 32]).expect("psk"),
            "Living Room".to_owned(),
        )
        .to_uri()
        .expect("encode pairing uri")
    }

    /// On a protocol mismatch the web bridge still rejects (strict check), AND it
    /// dispatches the loader's `tyde:repair-needed` event carrying the raw URI so
    /// the PWA loader can reboot into the version-matched bundle.
    #[wasm_bindgen_test]
    fn protocol_mismatch_rejects_and_dispatches_repair_needed() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let window = web_sys::window().expect("window");
        let uri = mismatched_protocol_uri();

        let captured: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let captured_cb = captured.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Ok(custom) = event.dyn_into::<web_sys::CustomEvent>() {
                *captured_cb.borrow_mut() = custom.detail().as_string();
            }
        });
        window
            .add_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("add listener");

        let result = parse_and_validate(&uri);
        assert!(result.is_err(), "strict protocol check still rejects");

        window
            .remove_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("remove listener");

        assert_eq!(
            captured.borrow().as_deref(),
            Some(uri.as_str()),
            "repair-needed must carry the raw pairing URI for the loader to reboot",
        );
    }

    /// A protocol-matching URI must NOT dispatch repair-needed.
    #[wasm_bindgen_test]
    fn matching_protocol_does_not_dispatch_repair_needed() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let window = web_sys::window().expect("window");
        let fired = Rc::new(RefCell::new(false));
        let fired_cb = fired.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
            *fired_cb.borrow_mut() = true;
        });
        window
            .add_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("add listener");

        let _ = parse_and_validate(&valid_uri()).expect("valid uri parses");

        window
            .remove_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("remove listener");

        assert!(!*fired.borrow(), "no repair on a matching protocol");
    }

    /// The reconnect self-heal dispatches `tyde:repair-version` carrying the
    /// host's release version so the loader can reboot an already-paired host
    /// into the version-matched bundle without a re-scan.
    #[wasm_bindgen_test]
    fn request_loader_repair_version_dispatches_event_with_version() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let window = web_sys::window().expect("window");
        let captured: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let captured_cb = captured.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Ok(custom) = event.dyn_into::<web_sys::CustomEvent>() {
                *captured_cb.borrow_mut() = custom.detail().as_string();
            }
        });
        window
            .add_event_listener_with_callback("tyde:repair-version", cb.as_ref().unchecked_ref())
            .expect("add listener");

        request_loader_repair_version("0.8.19-beta.15");

        window
            .remove_event_listener_with_callback("tyde:repair-version", cb.as_ref().unchecked_ref())
            .expect("remove listener");

        assert_eq!(
            captured.borrow().as_deref(),
            Some("0.8.19-beta.15"),
            "repair-version must carry the release version for the loader to reboot",
        );
    }

    /// A managed (`tyde-pair://v2`) offer classifies as connectable managed
    /// service, carrying the host label for the auth screen.
    #[wasm_bindgen_test]
    async fn classify_managed_offer_returns_managed_service() {
        let uri = tests_support::sample_managed_uri();
        match classify_pairing_offer(&uri)
            .await
            .expect("classify managed")
        {
            PairingOffer::ManagedService { host_label } => assert_eq!(host_label, "Living Room"),
            other => panic!("expected managed service, got {other:?}"),
        }
    }

    /// A legacy (`tyde-pair://v1`) public-broker offer classifies as repair
    /// required — never a connectable offer.
    #[wasm_bindgen_test]
    async fn classify_legacy_public_broker_offer_requires_repair() {
        let uri = tests_support::legacy_public_broker_uri();
        match classify_pairing_offer(&uri).await.expect("classify legacy") {
            PairingOffer::RepairRequired { message } => {
                assert!(!message.is_empty(), "repair message must be actionable");
                assert!(
                    message.contains("Settings → Mobile"),
                    "repair must point at the tab that contains pairing: {message}"
                );
                assert!(
                    message.contains("Mobile tab in Settings"),
                    "spoken wording must not depend on the arrow glyph: {message}"
                );
                assert!(
                    !message.contains("Settings → Hosts"),
                    "the Hosts tab is a dead-end recovery path: {message}"
                );
            }
            other => panic!("expected repair required, got {other:?}"),
        }
    }

    /// The `https://tycode.dev/tyde/#<managed-uri>` loader-wrapped form also
    /// classifies as managed service (the QR secret rides in the fragment).
    #[wasm_bindgen_test]
    async fn classify_accepts_https_fragment_managed_offer() {
        let uri = tests_support::sample_managed_uri();
        let wrapped = format!("https://tycode.dev/tyde/#{uri}");
        assert!(matches!(
            classify_pairing_offer(&wrapped).await,
            Ok(PairingOffer::ManagedService { .. })
        ));
    }

    /// A managed offer whose embedded protocol version mismatches this build is
    /// rejected (so this bundle never proceeds) and dispatches the loader
    /// self-heal reboot event.
    #[wasm_bindgen_test]
    async fn classify_managed_protocol_mismatch_rejects_and_requests_repair() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let window = web_sys::window().expect("window");
        let fired = Rc::new(RefCell::new(false));
        let fired_cb = fired.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
            *fired_cb.borrow_mut() = true;
        });
        window
            .add_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("add listener");

        let uri = tests_support::mismatched_protocol_managed_uri();
        assert!(classify_pairing_offer(&uri).await.is_err());

        window
            .remove_event_listener_with_callback("tyde:repair-needed", cb.as_ref().unchecked_ref())
            .expect("remove listener");
        assert!(
            *fired.borrow(),
            "a managed protocol mismatch must request a loader reboot",
        );
    }
}
