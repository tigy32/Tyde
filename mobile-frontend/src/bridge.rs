//! Backend abstraction for the mobile client's host I/O (transport, storage,
//! QR, dialogs), with two implementations selected **at runtime**:
//!
//! - [`tauri_backend`] — reaches the native `mobile-shell` Rust commands/events
//!   through `window.__TAURI__` (the iOS app shell).
//! - [`web`] — talks directly to the host from wasm: MQTT-over-wss transport,
//!   IndexedDB storage, browser-camera QR scan, in-process events (the PWA).
//!
//! ## Why runtime-detect (not a cargo feature split)
//!
//! `mobile-frontend` ships as a **single** `trunk`-built wasm bundle. The same
//! bundle runs inside the Tauri WKWebView (where `window.__TAURI__` is injected)
//! *and* as a standalone browser PWA (where it is not). The Tauri build is
//! produced by `trunk build` with no feature flags (see
//! `mobile/src-tauri/tauri.conf.json`), so a feature split would force a second
//! artifact and a second build pipeline. Branching on the presence of
//! `window.__TAURI__` keeps one artifact and is the lowest-churn option: every
//! call site below is unchanged, and the choice is a single cached boolean.

mod tauri_backend;
mod web;

use serde::Deserialize;

pub use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
pub use mobile_shell_types::{
    KnownConnectionInstance, PairedHostConnectionStatusEvent, PairedHostsChangedEvent,
};

pub use web::{AuthProvider, RedeemOutcome};

use crate::state::{
    LocalHostId, MobileServiceAuthState, MobileShellError, PairedHostSummary, PairingOffer,
};

/// Result of a QR scan. Shared by both backends (Tauri decodes the native
/// barcode-scanner result into it; web fills it from `BarcodeDetector`).
#[derive(Deserialize)]
pub struct BarcodeScanResult {
    pub content: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub format: Option<String>,
}

/// Opaque listener handle. Both backends produce one; `remove()` unregisters.
/// `app.rs` deliberately `std::mem::forget`s these for the app's lifetime, so
/// the captured listener stays alive until the page/webview is torn down.
pub struct UnlistenHandle {
    cleanup: Box<dyn FnOnce()>,
}

impl UnlistenHandle {
    fn from_cleanup(cleanup: impl FnOnce() + 'static) -> Self {
        Self {
            cleanup: Box::new(cleanup),
        }
    }

    #[allow(dead_code)]
    pub fn remove(self) {
        (self.cleanup)();
    }
}

/// True when the bundle is running as a browser PWA (no Tauri shell injected).
/// Cached: the host environment cannot change within a page lifetime. The
/// detected mode is logged once on first use so a future `__TAURI__`-injected-
/// late misdetection (web chosen when Tauri was expected, or vice-versa) is
/// visible in the console rather than silently changing every host call.
#[cfg(not(all(test, target_arch = "wasm32")))]
fn use_web_backend() -> bool {
    thread_local! {
        static IS_WEB: bool = {
            let is_web = !tauri_present();
            log::info!(
                "mobile bridge backend selected: {}",
                if is_web { "web (browser PWA)" } else { "tauri (native shell)" }
            );
            is_web
        };
    }
    IS_WEB.with(|is_web| *is_web)
}

#[cfg(all(test, target_arch = "wasm32"))]
fn use_web_backend() -> bool {
    !tauri_present()
}

fn tauri_present() -> bool {
    web_sys::window()
        .and_then(|window| {
            js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("__TAURI__")).ok()
        })
        .map(|value| !value.is_undefined() && !value.is_null())
        .unwrap_or(false)
}

/// Dispatches each bridge call to the web or Tauri backend. Kept terse: every
/// arm is `if use_web_backend() { web::f(..) } else { tauri_backend::f(..) }`.
macro_rules! dispatch {
    ($name:ident ( $( $arg:ident : $ty:ty ),* ) -> $ret:ty) => {
        pub async fn $name( $( $arg : $ty ),* ) -> $ret {
            if use_web_backend() {
                web::$name( $( $arg ),* ).await
            } else {
                tauri_backend::$name( $( $arg ),* ).await
            }
        }
    };
}

dispatch!(list_paired_hosts() -> Result<Vec<PairedHostSummary>, String>);
dispatch!(list_paired_host_connection_statuses() -> Result<Vec<PairedHostConnectionStatusEvent>, String>);
dispatch!(list_pending_host_lines() -> Result<Vec<HostLineEvent>, String>);
dispatch!(classify_pairing_offer(qr_uri: &str) -> Result<PairingOffer, String>);
dispatch!(start_pairing(qr_uri: &str) -> Result<(), String>);
dispatch!(connect_paired_host(local_host_id: &LocalHostId) -> Result<(), String>);
dispatch!(disconnect_paired_host(local_host_id: &LocalHostId) -> Result<(), String>);
dispatch!(forget_paired_host(local_host_id: &LocalHostId) -> Result<(), String>);
dispatch!(send_host_line(local_host_id: &LocalHostId, line: &str) -> Result<(), String>);
dispatch!(ack_host_line(local_host_id: &LocalHostId, delivery_id: u64) -> Result<(), String>);
dispatch!(scan_qr() -> Result<BarcodeScanResult, String>);
dispatch!(ensure_camera_permission() -> Result<(), String>);

pub async fn set_paired_host_auto_connect(
    local_host_id: &LocalHostId,
    auto_connect: bool,
) -> Result<(), String> {
    if use_web_backend() {
        web::set_paired_host_auto_connect(local_host_id, auto_connect).await
    } else {
        tauri_backend::set_paired_host_auto_connect(local_host_id, auto_connect).await
    }
}

/// Web-only: consumes the pairing URI the PWA loader stashed in sessionStorage
/// (see [`web::take_pending_pairing_uri`]) so first-time pairing completes after
/// the loader boots this bundle. Native shells deliver pairing URIs through
/// their own scan/paste flow, so this is always `None` there. Synchronous: a
/// sessionStorage read needs no await.
pub fn take_pending_pairing_uri() -> Option<String> {
    if use_web_backend() {
        web::take_pending_pairing_uri()
    } else {
        None
    }
}

/// Web-only: ask the PWA loader to reboot into the host's exact published
/// bundle (identified by `release_version`) after an incompatible-protocol
/// reject, so an already-paired host self-heals without a re-scan. Native
/// shells have no loader to reboot, so this is a no-op there and the sticky
/// `UpdateRequired` error is the surface. Synchronous: it just dispatches the
/// DOM `tyde:repair-version` CustomEvent the loader listens for.
pub fn request_loader_repair_version(release_version: &str) {
    if use_web_backend() {
        web::request_loader_repair_version(release_version);
    }
}

/// Web-only: run the `tycode.dev` Tyggs auth (`POST /auth/session`) step of the
/// managed pairing flow and return the resulting typed auth state. The native
/// shell owns its own managed handshake and never reaches this path, so it
/// returns a non-retryable `service_unavailable` rather than pretending to
/// authenticate.
pub async fn authenticate_managed(qr_uri: &str) -> MobileServiceAuthState {
    if use_web_backend() {
        web::authenticate_managed(qr_uri).await
    } else {
        MobileServiceAuthState::ServiceUnavailable {
            message: "Managed pairing is handled by the native app shell.".to_owned(),
            retryable: false,
        }
    }
}

/// Web-only: resolve the OAuth callback that returned to this app at boot. The
/// web service coordinates this with reconnect credential minting so both paths
/// observe one exchange result instead of racing for the one-time marker.
pub async fn complete_boot_managed_auth_callback() -> Option<MobileServiceAuthState> {
    if use_web_backend() {
        web::complete_boot_managed_auth_callback().await
    } else {
        None
    }
}

/// Web-only: re-probe the current cookie-backed managed session without a QR.
/// Used by the no-pending-QR pass/paywall and retryable-service screens.
pub async fn probe_managed_auth() -> MobileServiceAuthState {
    if use_web_backend() {
        web::probe_managed_auth().await
    } else {
        MobileServiceAuthState::ServiceUnavailable {
            message: "Managed authentication is handled by the native app shell.".to_owned(),
            retryable: false,
        }
    }
}

/// Web-only: redeem a managed offer (`POST /pairings/redeem`) and connect to the
/// managed broker. Native shells never reach this path (they produce a
/// `DirectPairing` offer), so it returns a terminal outcome there.
pub async fn redeem_managed_and_connect(qr_uri: &str) -> Result<(), RedeemOutcome> {
    if use_web_backend() {
        web::redeem_managed_and_connect(qr_uri).await
    } else {
        Err(RedeemOutcome::Terminal {
            message: "Managed pairing is handled by the native app shell.".to_owned(),
        })
    }
}

/// Web-only: start the Tyggs sign-in through `tycode.dev` (see
/// [`web::begin_tyggs_sign_in`]) for an explicitly selected configured
/// provider. Pass the scanned pairing URI to resume pairing after the redirect,
/// or `None` to just re-authenticate an existing host. Native shells own their
/// own sign-in, so this errors there.
pub fn tyggs_auth_providers() -> Result<Vec<AuthProvider>, String> {
    if use_web_backend() {
        web::tyggs_auth_providers()
    } else {
        Err("Tyggs sign-in is handled by the native app shell.".to_owned())
    }
}

pub fn begin_tyggs_sign_in(
    provider: AuthProvider,
    resume_qr_uri: Option<&str>,
) -> Result<(), String> {
    if use_web_backend() {
        web::begin_tyggs_sign_in(provider, resume_qr_uri)
    } else {
        Err("Tyggs sign-in is handled by the native app shell.".to_owned())
    }
}

pub async fn frontend_attached(
    known_connection_instance_ids: &[KnownConnectionInstance],
) -> Result<(), String> {
    if use_web_backend() {
        web::frontend_attached(known_connection_instance_ids).await
    } else {
        tauri_backend::frontend_attached(known_connection_instance_ids).await
    }
}

#[allow(dead_code)]
pub async fn wasm_log(level: &str, message: &str) {
    if use_web_backend() {
        web::wasm_log(level, message).await
    } else {
        tauri_backend::wasm_log(level, message).await
    }
}

/// Generates the event-listener façade fns. Each registers the callback with
/// whichever backend is active and returns a unified [`UnlistenHandle`].
macro_rules! dispatch_listen {
    ($name:ident, $payload:ty) => {
        pub async fn $name(
            callback: impl Fn($payload) + 'static,
        ) -> Result<UnlistenHandle, String> {
            if use_web_backend() {
                web::$name(callback).await
            } else {
                tauri_backend::$name(callback).await
            }
        }
    };
}

dispatch_listen!(listen_host_line, HostLineEvent);
dispatch_listen!(listen_host_disconnected, HostDisconnectedEvent);
dispatch_listen!(listen_host_error, HostErrorEvent);
dispatch_listen!(listen_paired_hosts_changed, PairedHostsChangedEvent);
dispatch_listen!(
    listen_paired_host_connection_status,
    PairedHostConnectionStatusEvent
);
dispatch_listen!(listen_mobile_shell_error, MobileShellError);
