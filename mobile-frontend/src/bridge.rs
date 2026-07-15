//! Web/PWA bridge for the mobile client's host I/O, storage, QR scanning, and
//! dialogs. The browser talks directly to hosts over MQTT-over-WebSocket,
//! persists pairing data in IndexedDB, and delivers events in process.

mod web;

use serde::{Deserialize, Serialize};

pub use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
pub use mobile_shell_types::{
    KnownConnectionInstance, PairedHostConnectionStatusEvent, PairedHostsChangedEvent,
};

pub use web::{AuthProvider, RedeemOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalSubmissionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub connection_instance_id: u64,
    pub local_submission_id: LocalSubmissionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRejected {
    NotConnected,
    QueueFull,
    ConnectionClosed,
}

impl std::fmt::Display for SendRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("host is not connected"),
            Self::QueueFull => f.write_str("host outbound queue is full"),
            Self::ConnectionClosed => f.write_str("host connection closed"),
        }
    }
}

impl std::error::Error for SendRejected {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionTransportOutcome {
    QueuedLocally,
    NotSent,
    BrokerAcknowledged,
    DeliveryUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionTransportOutcomeEvent {
    pub local_host_id: LocalHostId,
    pub connection_instance_id: u64,
    pub local_submission_id: LocalSubmissionId,
    pub outcome: SubmissionTransportOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionInvalidation {
    SequenceViolation { message: String },
    ProtocolViolation { message: String },
}

impl std::fmt::Display for ConnectionInvalidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceViolation { message } => {
                write!(f, "sequence validation failed: {message}")
            }
            Self::ProtocolViolation { message } => {
                write!(f, "protocol validation failed: {message}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationRejected {
    NotConnected,
    ConnectionClosed,
}

impl std::fmt::Display for InvalidationRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("host is not connected"),
            Self::ConnectionClosed => f.write_str("host connection closed"),
        }
    }
}

impl std::error::Error for InvalidationRejected {}

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) use web::{
    TestSendGuard, test_capture_sends, test_clean_sends, test_defer_sends, test_reject_sends,
    test_resolve_next_send, test_send_attempts, test_sent_lines,
};

use crate::state::{
    LocalHostId, MobileServiceAuthState, MobileShellError, PairedHostSummary, PairingOffer,
};

/// Result of a browser-camera QR scan.
#[derive(Deserialize)]
pub struct BarcodeScanResult {
    pub content: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub format: Option<String>,
}

/// Opaque listener handle. `remove()` unregisters the web listener.
/// `app.rs` deliberately `std::mem::forget`s these for the app's lifetime, so
/// the captured listener stays alive until the page is torn down.
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

macro_rules! dispatch {
    ($name:ident ( $( $arg:ident : $ty:ty ),* ) -> $ret:ty) => {
        pub async fn $name( $( $arg : $ty ),* ) -> $ret {
            web::$name( $( $arg ),* ).await
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
dispatch!(send_host_line(local_host_id: &LocalHostId, line: &str) -> Result<Accepted, SendRejected>);
dispatch!(ack_host_line(local_host_id: &LocalHostId, delivery_id: u64) -> Result<(), String>);
dispatch!(scan_qr() -> Result<BarcodeScanResult, String>);
dispatch!(ensure_camera_permission() -> Result<(), String>);

pub async fn set_paired_host_auto_connect(
    local_host_id: &LocalHostId,
    auto_connect: bool,
) -> Result<(), String> {
    web::set_paired_host_auto_connect(local_host_id, auto_connect).await
}

pub fn invalidate_host_connection(
    local_host_id: &LocalHostId,
    reason: ConnectionInvalidation,
) -> Result<(), InvalidationRejected> {
    web::invalidate_host_connection(local_host_id, reason)
}

/// Consumes the pairing URI the PWA loader stashed in sessionStorage
/// (see [`web::take_pending_pairing_uri`]) so first-time pairing completes after
/// the loader boots this bundle. Synchronous: a sessionStorage read needs no
/// await.
pub fn take_pending_pairing_uri() -> Option<String> {
    web::take_pending_pairing_uri()
}

/// Asks the PWA loader to reboot into the host's exact published
/// bundle (identified by `release_version`) after an incompatible-protocol
/// reject, so an already-paired host self-heals without a re-scan. Synchronous:
/// it dispatches the DOM `tyde:repair-version` CustomEvent the loader listens
/// for.
pub fn request_loader_repair_version(release_version: &str) {
    web::request_loader_repair_version(release_version);
}

/// Runs the `tycode.dev` Tyggs auth (`POST /auth/session`) step of the managed
/// pairing flow and returns the resulting typed auth state.
pub async fn authenticate_managed(qr_uri: &str) -> MobileServiceAuthState {
    web::authenticate_managed(qr_uri).await
}

/// Resolves the OAuth callback that returned to this app at boot. The
/// web service coordinates this with reconnect credential minting so both paths
/// observe one exchange result instead of racing for the one-time marker.
pub async fn complete_boot_managed_auth_callback() -> Option<MobileServiceAuthState> {
    web::complete_boot_managed_auth_callback().await
}

/// Re-probes the current cookie-backed managed session without a QR.
/// Used by the no-pending-QR pass/paywall and retryable-service screens.
pub async fn probe_managed_auth() -> MobileServiceAuthState {
    web::probe_managed_auth().await
}

/// Redeems a managed offer (`POST /pairings/redeem`) and connects to the
/// managed broker.
pub async fn redeem_managed_and_connect(qr_uri: &str) -> Result<(), RedeemOutcome> {
    web::redeem_managed_and_connect(qr_uri).await
}

/// Starts Tyggs sign-in through `tycode.dev` (see
/// [`web::begin_tyggs_sign_in`]) for an explicitly selected configured
/// provider. Pass the scanned pairing URI to resume pairing after the redirect,
/// or `None` to just re-authenticate an existing host.
pub fn tyggs_auth_providers() -> Result<Vec<AuthProvider>, String> {
    web::tyggs_auth_providers()
}

pub fn begin_tyggs_sign_in(
    provider: AuthProvider,
    resume_qr_uri: Option<&str>,
) -> Result<(), String> {
    web::begin_tyggs_sign_in(provider, resume_qr_uri)
}

pub async fn frontend_attached(
    known_connection_instance_ids: &[KnownConnectionInstance],
) -> Result<(), String> {
    web::frontend_attached(known_connection_instance_ids).await
}

#[allow(dead_code)]
pub async fn wasm_log(level: &str, message: &str) {
    web::wasm_log(level, message).await
}

/// Generates the event-listener façade functions.
macro_rules! dispatch_listen {
    ($name:ident, $payload:ty) => {
        pub async fn $name(
            callback: impl Fn($payload) + 'static,
        ) -> Result<UnlistenHandle, String> {
            web::$name(callback).await
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
dispatch_listen!(
    listen_submission_transport_outcome,
    SubmissionTransportOutcomeEvent
);
