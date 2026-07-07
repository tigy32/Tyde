//! Browser (PWA) `tycode.dev` managed mobile-access client.
//!
//! The managed pairing flow (`tyde-pair://v2`) runs a **pre-transport** sequence
//! against `tycode.dev` before any MQTT credential exists (see
//! `dev-docs/30-mobile-managed-broker.md`):
//!
//! 1. The user signs in with Tyggs **through `tycode.dev`** via a full-page
//!    redirect to `GET /auth/start?provider=<provider>&return_to=<app-url>`. The
//!    service runs the Tyggs OAuth dance server-side and redirects the browser
//!    back to `return_to` (this app) carrying a short-lived, one-time
//!    `handoffCode` marker. The mobile app never sees Tyggs OAuth access tokens
//!    or pass proofs (locked decision #6) — they live only between the browser,
//!    Tyggs, and `tycode.dev`.
//! 2. On return the app strips the `handoffCode` from the URL **immediately**
//!    (so a reload/back-button can't replay it) and exchanges it via
//!    `POST /auth/session` `{ handoff_code, client: { kind: "mobile_web",
//!    release_version, protocol_version } }`. On success `tycode.dev` sets a
//!    first-party, `HttpOnly` `tycode_mobile_session` cookie. A cookie-only
//!    `GET /auth/session` (no handoff code) re-probes an already-established
//!    session on later loads (`authenticated` / `pass_required` /
//!    `mobile_session_required`).
//! 3. `POST /pairings/redeem` (cookie) consumes the scanned offer and returns the
//!    durable `device_pairing_secret` plus mobile-scoped AWS IoT broker
//!    credentials.
//! 4. `POST /pairings/{id}/broker-credentials` (cookie + HMAC over
//!    `device_pairing_secret`) mints fresh short-lived broker credentials on each
//!    (re)connect.
//!
//! Every request uses `credentials: "include"` so the session cookie rides along;
//! **no** auth secret is ever read from a JS global or held in JS memory (the
//! `handoffCode` is a one-time redirect marker, not a stored token — it is
//! consumed and dropped from the URL on first read). The connection manager only
//! ever receives service-issued [`mqtt_transport::ManagedMqttConnectConfig`]
//! material, and ephemeral broker grants are cached only in memory — never
//! written to IndexedDB.
//!
//! ## Configuration
//!
//! Runtime config lives on `window.__TYDE_MOBILE_SERVICE__`:
//!
//! ```js
//! window.__TYDE_MOBILE_SERVICE__ = {
//!   baseUrl: "https://tycode.dev/api/tyde/mobile/v1",   // enables the live client
//!   provider: "google",                                 // "google" | "apple"
//!   // Dev/test-only deterministic outcomes (used by wasm tests):
//!   stubAuth: "authenticated" | "pass_required" | "auth_failed" | "service_unavailable",
//!   stubRedeem: "ok" | "repair_required" | "pass_required" | "service_unavailable",
//!   paywallUrl: "https://tyggs.com/pass",
//! };
//! ```
//!
//! With no `baseUrl` and no stub the client **fails closed** — it never falls
//! back to a public/free broker (locked decision #8) and never fabricates a
//! session — so a real managed QR in an unprovisioned build surfaces an explicit
//! `service_unavailable`, not a spinner or a silent legacy connect.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mobile_shell_types::LocalHostId;
use mqtt_transport::{MQTT_TRANSPORT_PROTOCOL_VERSION, ManagedMobilePairingQrPayload};
use protocol::{
    ManagedBrokerAuthorizerName, ManagedBrokerClientId, ManagedBrokerConnectAuth,
    ManagedBrokerCredentialScope, ManagedBrokerCredentials, ManagedBrokerEndpoint,
    ManagedBrokerGrantId, ManagedBrokerProvider, ManagedBrokerRegion, ManagedBrokerRole,
    ManagedBrokerTopicNamespace, MobileAccessErrorCode, MobileServiceAuthState, PROTOCOL_VERSION,
    TYDE_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::store::{
    IndexedDbHostStore, IndexedDbPskStore, ManagedPairingRecord, PskStore, WebPairedHostRecord,
};

/// Signing prefix + auth header — MUST stay byte-identical to the host signer in
/// `server::mobile_access` (`PAIRING_HMAC_PREFIX` / `pairing_auth_header`) so the
/// `tycode.dev` HMAC verifier accepts both host and mobile callers.
const PAIRING_HMAC_PREFIX: &str = "TYCODE-PAIRING-HMAC-V1";
const PAIRING_AUTH_HEADER: &str = "x-tycode-pairing-auth";
const CONFIG_GLOBAL: &str = "__TYDE_MOBILE_SERVICE__";
/// Provider passed to `GET /auth/start?provider=…` when the config global does
/// not override it. This must be one of the OAuth providers accepted by the
/// service/Tyggs OAuth flow.
const DEFAULT_AUTH_PROVIDER: AuthProvider = AuthProvider::Google;
/// One-time marker `tycode.dev` appends to `return_to` after the Tyggs OAuth
/// dance. Read from the query string or the URL fragment, stripped immediately,
/// and exchanged for the session cookie via `POST /auth/session`.
const HANDOFF_CODE_KEY: &str = "handoffCode";
/// Refresh managed broker credentials this long before they expire so a
/// reconnect never hands the transport an already-dead grant.
const CREDENTIAL_REFRESH_MARGIN_MS: u64 = 30_000;

/// Terminal or re-renderable outcome of a redeem attempt. `Auth` re-drives the
/// [`MobileServiceAuthState`] card (e.g. the session lapsed into `pass_required`
/// mid-flow); `Repair` and `Terminal` are dead ends the pairing flow renders as
/// the repair-required / failed screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemOutcome {
    Auth(MobileServiceAuthState),
    Repair { message: String },
    Terminal { message: String },
}

/// Failure obtaining managed broker credentials for a (re)connect. Surfaced by
/// the connection manager as a typed, terminal-or-retryable connect error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCredentialError {
    pub code: MobileAccessErrorCode,
    pub message: String,
    pub retryable: bool,
}

// ── In-memory session marker + ephemeral credential cache ───────────────────

thread_local! {
    /// Local echo of the `tycode.dev` session state so a stubbed redeem knows it
    /// is "authenticated". The authoritative session is the `HttpOnly` cookie the
    /// browser holds — this is not a secret and is never sent anywhere.
    static AUTHENTICATED: Cell<bool> = const { Cell::new(false) };

    /// Fresh mobile broker grants, held **only in memory** (never persisted —
    /// finding #5 / dev-docs/30). Populated by redeem and mint; reused for a
    /// reconnect that happens before they expire; dropped on forget or reload.
    static CREDENTIAL_CACHE: RefCell<HashMap<LocalHostId, ManagedBrokerCredentials>> =
        RefCell::new(HashMap::new());
}

fn mark_authenticated(value: bool) {
    AUTHENTICATED.with(|flag| flag.set(value));
}

fn is_authenticated() -> bool {
    AUTHENTICATED.with(Cell::get)
}

fn cache_credentials(local_host_id: &LocalHostId, credentials: ManagedBrokerCredentials) {
    CREDENTIAL_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .insert(local_host_id.clone(), credentials);
    });
}

fn cached_fresh_credentials(
    local_host_id: &LocalHostId,
    now_ms: u64,
) -> Option<ManagedBrokerCredentials> {
    CREDENTIAL_CACHE.with(|cache| {
        cache
            .borrow()
            .get(local_host_id)
            .filter(|credentials| credentials_are_fresh(credentials, now_ms))
            .cloned()
    })
}

/// Drops the in-memory broker grant for a forgotten host so it can't be reused.
pub fn clear_cached_credentials(local_host_id: &LocalHostId) {
    CREDENTIAL_CACHE.with(|cache| {
        cache.borrow_mut().remove(local_host_id);
    });
}

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthProvider {
    Google,
    Apple,
}

impl AuthProvider {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "google" => Ok(Self::Google),
            "apple" => Ok(Self::Apple),
            other => Err(format!("unsupported Tyggs OAuth provider {other:?}")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Apple => "apple",
        }
    }
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    base_url: Option<String>,
    provider: Result<AuthProvider, String>,
    stub_auth: Option<String>,
    stub_redeem: Option<String>,
    paywall_url: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            provider: Ok(DEFAULT_AUTH_PROVIDER),
            stub_auth: None,
            stub_redeem: None,
            paywall_url: None,
        }
    }
}

fn read_string_field(config: &JsValue, field: &str) -> Option<String> {
    js_sys::Reflect::get(config, &JsValue::from_str(field))
        .ok()
        .and_then(|value| value.as_string())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn load_config() -> ServiceConfig {
    let Some(window) = web_sys::window() else {
        return ServiceConfig::default();
    };
    let config = match js_sys::Reflect::get(&window, &JsValue::from_str(CONFIG_GLOBAL)) {
        Ok(value) if !value.is_undefined() && !value.is_null() => value,
        _ => return ServiceConfig::default(),
    };
    ServiceConfig {
        base_url: read_string_field(&config, "baseUrl")
            .map(|url| url.trim_end_matches('/').to_owned()),
        provider: read_string_field(&config, "provider")
            .map(|provider| AuthProvider::parse(&provider))
            .unwrap_or(Ok(DEFAULT_AUTH_PROVIDER)),
        stub_auth: read_string_field(&config, "stubAuth"),
        stub_redeem: read_string_field(&config, "stubRedeem"),
        paywall_url: read_string_field(&config, "paywallUrl"),
    }
}

fn paywall_url(config: &ServiceConfig) -> String {
    config
        .paywall_url
        .clone()
        .unwrap_or_else(|| "https://tyggs.com/pass".to_owned())
}

fn not_configured() -> MobileServiceAuthState {
    MobileServiceAuthState::ServiceUnavailable {
        message: "Tyde managed mobile access isn't set up in this build yet. \
                  This build has no tycode.dev endpoint configured, so it can't \
                  sign in or redeem this pairing."
            .to_owned(),
        retryable: false,
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Resolves the `tycode.dev` session and returns the typed
/// [`MobileServiceAuthState`]. If the browser just returned from the OAuth
/// redirect it first completes the one-time `handoffCode` exchange
/// ([`complete_handoff`]); otherwise it probes the existing session cookie
/// (`GET /auth/session`). An `AuthFailed` result means "not signed in" — the UI
/// drives [`tyggs_sign_in_url`] to start the redirect.
pub async fn authenticate(qr_uri: &str) -> MobileServiceAuthState {
    if let Err(message) = parse_managed_offer(qr_uri) {
        return MobileServiceAuthState::AuthFailed { message };
    }
    let config = load_config();
    if let Some(base_url) = config.base_url.clone() {
        return authenticate_live(&base_url, &config).await;
    }
    match config.stub_auth.as_deref() {
        Some(kind) => stub_auth_state(kind, &config),
        None => not_configured(),
    }
}

/// The full-page URL that starts the Tyggs sign-in through `tycode.dev`. `None`
/// when no service endpoint is configured (so the UI fails closed instead of
/// navigating nowhere). `provider` selects the OAuth provider (defaults to
/// [`DEFAULT_AUTH_PROVIDER`]); `return_to` brings the browser back to the current
/// app URL, where the appended one-time `handoffCode` is exchanged for the
/// session cookie (see [`complete_handoff`]).
pub fn tyggs_sign_in_url() -> Option<String> {
    let config = load_config();
    let base_url = config.base_url.clone()?;
    let provider = match config.provider {
        Ok(provider) => provider,
        Err(message) => {
            log::error!("invalid managed mobile auth provider: {message}");
            return None;
        }
    };
    let return_to = sanitized_return_to_url()?;
    let encoded_provider = js_sys::encode_uri_component(provider.as_str());
    let encoded_return = js_sys::encode_uri_component(&return_to);
    Some(format!(
        "{base_url}/auth/start?provider={encoded_provider}&return_to={encoded_return}"
    ))
}

fn sanitized_return_to_url() -> Option<String> {
    let window = web_sys::window()?;
    let href = window.location().href().ok()?;
    let url = web_sys::Url::new(&href).ok()?;
    url.set_protocol("https:");
    url.set_hash("");
    for key in [
        HANDOFF_CODE_KEY,
        "offer_secret",
        "offerSecret",
        "room",
        "psk",
    ] {
        url.search_params().delete(key);
    }
    Some(url.href())
}

/// Redeems the scanned offer against `tycode.dev` (`POST /pairings/redeem`,
/// cookie-authenticated), persists the durable managed pairing, and connects to
/// the managed broker. Returns a [`RedeemOutcome`] on failure — never a fallback
/// to a legacy/public broker.
pub async fn redeem_and_connect(qr_uri: &str) -> Result<(), RedeemOutcome> {
    let offer =
        parse_managed_offer(qr_uri).map_err(|message| RedeemOutcome::Terminal { message })?;
    let config = load_config();
    if let Some(base_url) = config.base_url.clone() {
        return redeem_live(&base_url, &offer).await;
    }
    match config.stub_redeem.as_deref() {
        Some("ok") if is_authenticated() => finish_redeem(&offer, stub_redeem_result(&offer)).await,
        Some("ok") => Err(RedeemOutcome::Auth(MobileServiceAuthState::AuthFailed {
            message: "Sign in with your Tyggs account before redeeming.".to_owned(),
        })),
        Some("repair_required") => Err(RedeemOutcome::Repair {
            message: "This pairing needs to be repaired. Re-pair from the host's \
                      current QR code."
                .to_owned(),
        }),
        Some("pass_required") => Err(RedeemOutcome::Auth(MobileServiceAuthState::PassRequired {
            message: "A Tyggs Pass is required for Tyde mobile access.".to_owned(),
            paywall_url: paywall_url(&config),
        })),
        Some("service_unavailable") => Err(RedeemOutcome::Auth(
            MobileServiceAuthState::ServiceUnavailable {
                message: "tycode.dev is temporarily unavailable. Try again in a moment.".to_owned(),
                retryable: true,
            },
        )),
        _ => Err(RedeemOutcome::Auth(not_configured())),
    }
}

/// Obtains managed broker credentials for a (re)connect: reuses the in-memory
/// cached grant while it is still fresh, otherwise mints new ones via
/// `POST /pairings/{id}/broker-credentials` (cookie + HMAC). Used by the
/// connection manager to build the `ManagedMqttConnectConfig`.
pub async fn obtain_managed_credentials(
    record: &WebPairedHostRecord,
    now_ms: u64,
) -> Result<(ManagedBrokerEndpoint, ManagedBrokerCredentials), ManagedCredentialError> {
    let managed = record
        .managed
        .as_ref()
        .ok_or_else(|| ManagedCredentialError {
            code: MobileAccessErrorCode::RepairRequired,
            message: "paired host has no managed tycode.dev identity".to_owned(),
            retryable: false,
        })?;
    if let Some(credentials) = cached_fresh_credentials(&record.local_host_id, now_ms) {
        return Ok((managed.broker.clone(), credentials));
    }
    mint_managed_credentials(record, managed).await
}

// ── Live HTTP: auth ──────────────────────────────────────────────────────────

async fn authenticate_live(base_url: &str, config: &ServiceConfig) -> MobileServiceAuthState {
    // Fresh back from the `tycode.dev` OAuth redirect: a one-time `handoffCode`
    // is in the URL. Strip it immediately and exchange it for the session cookie
    // before falling back to the cookie-only probe below.
    if let Some(handoff_code) = take_handoff_code() {
        return complete_handoff(base_url, config, &handoff_code).await;
    }
    let url = format!("{base_url}/auth/session");
    match send(HttpMethod::Get, &url, None, &[]).await {
        Ok(HttpJson { status, body }) if status < 300 => {
            match serde_json::from_str::<SessionResponse>(&body) {
                Ok(session) if session.authenticated => {
                    mark_authenticated(true);
                    MobileServiceAuthState::Authenticated {
                        expires_at_ms: session.expires_at_ms.unwrap_or_default(),
                    }
                }
                // A 2xx that reports "not authenticated" is the signed-out state.
                Ok(_) => {
                    mark_authenticated(false);
                    sign_in_required()
                }
                Err(err) => MobileServiceAuthState::ServiceUnavailable {
                    message: format!("tycode.dev returned an unreadable session response: {err}"),
                    retryable: true,
                },
            }
        }
        Ok(HttpJson { body, .. }) => {
            mark_authenticated(false);
            auth_state_from_error(&body, config)
        }
        Err(message) => MobileServiceAuthState::ServiceUnavailable {
            message,
            retryable: true,
        },
    }
}

/// The "you need to sign in with Tyggs" state. Encoded as `AuthFailed` because
/// the protocol enum has no dedicated variant yet (see the reported contract
/// gap); the UI renders it with a "Sign in with Tyggs" action, not a bare retry.
fn sign_in_required() -> MobileServiceAuthState {
    MobileServiceAuthState::AuthFailed {
        message: "Sign in with your Tyggs account to continue.".to_owned(),
    }
}

/// Exchanges a one-time `handoffCode` (already stripped from the URL) for the
/// `HttpOnly` `tycode_mobile_session` cookie via `POST /auth/session`. The
/// request carries no Tyggs secret — only the handoff marker plus this build's
/// client identity — so `tycode.dev` completes the session server-side.
async fn complete_handoff(
    base_url: &str,
    config: &ServiceConfig,
    handoff_code: &str,
) -> MobileServiceAuthState {
    let body = AuthSessionRequest {
        handoff_code: handoff_code.to_owned(),
        client: AuthClient {
            kind: "mobile_web",
            release_version: TYDE_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
        },
    };
    let bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        Err(err) => {
            return MobileServiceAuthState::ServiceUnavailable {
                message: format!("failed to serialize auth session request: {err}"),
                retryable: true,
            };
        }
    };
    let url = format!("{base_url}/auth/session");
    match send(HttpMethod::Post, &url, Some(&bytes), &[]).await {
        Ok(HttpJson { status, body }) if status < 300 => {
            match serde_json::from_str::<SessionResponse>(&body) {
                Ok(session) if session.authenticated => {
                    mark_authenticated(true);
                    MobileServiceAuthState::Authenticated {
                        expires_at_ms: session.expires_at_ms.unwrap_or_default(),
                    }
                }
                Ok(_) => {
                    mark_authenticated(false);
                    sign_in_required()
                }
                Err(err) => MobileServiceAuthState::ServiceUnavailable {
                    message: format!("tycode.dev returned an unreadable session response: {err}"),
                    retryable: true,
                },
            }
        }
        Ok(HttpJson { body, .. }) => {
            mark_authenticated(false);
            auth_state_from_error(&body, config)
        }
        Err(message) => MobileServiceAuthState::ServiceUnavailable {
            message,
            retryable: true,
        },
    }
}

/// Reads the one-time `handoffCode` marker from the current URL (query string or
/// fragment) and **removes it immediately** via `history.replaceState`, before
/// any network call, so a reload or back-button can never replay it. Returns
/// `None` when no handoff is present (the normal cookie-probe / reconnect path).
fn take_handoff_code() -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let href = location.href().ok()?;
    let url = web_sys::Url::new(&href).ok()?;

    // Query string form: `…/tyde/?handoffCode=…`. Mutating the associated
    // `URLSearchParams` updates `url.href` in place.
    if let Some(code) = url
        .search_params()
        .get(HANDOFF_CODE_KEY)
        .filter(|value| !value.is_empty())
    {
        url.search_params().delete(HANDOFF_CODE_KEY);
        url.set_hash("");
        replace_url(&window, &url);
        return Some(code);
    }

    // Fragment form: `…/tyde/#handoffCode=…` (or `…#other&handoffCode=…`).
    let hash = url.hash();
    let fragment = hash.strip_prefix('#').unwrap_or(&hash);
    if fragment.is_empty() {
        return None;
    }
    let mut found: Option<String> = None;
    for part in fragment.split('&') {
        if let Some((key, value)) = part.split_once('=')
            && key == HANDOFF_CODE_KEY
            && !value.is_empty()
        {
            found = Some(decode_uri_component(value));
        }
    }
    let code = found?;
    url.set_hash("");
    replace_url(&window, &url);
    Some(code)
}

/// Rewrites the address bar to `url` without navigating (so the stripped
/// handoff never lingers in history). Best-effort: if `History.replaceState` is
/// unavailable the code has still been consumed from memory.
fn replace_url(window: &web_sys::Window, url: &web_sys::Url) {
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&url.href()));
    }
}

fn decode_uri_component(value: &str) -> String {
    js_sys::decode_uri_component(value)
        .ok()
        .and_then(|decoded| decoded.as_string())
        .unwrap_or_else(|| value.to_owned())
}

// ── Live HTTP: redeem ─────────────────────────────────────────────────────────

async fn redeem_live(
    base_url: &str,
    offer: &ManagedMobilePairingQrPayload,
) -> Result<(), RedeemOutcome> {
    let body = RedeemRequest {
        offer_id: offer.offer_id.as_str().to_owned(),
        offer_secret: offer.offer_secret.clone(),
        device_label: "Tyde Mobile".to_owned(),
        device_nonce: nonce(),
        release_version: TYDE_VERSION.to_string(),
        protocol_version: PROTOCOL_VERSION,
        transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
    };
    let bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        Err(err) => {
            return Err(RedeemOutcome::Terminal {
                message: format!("failed to serialize redeem request: {err}"),
            });
        }
    };
    let url = format!("{base_url}/pairings/redeem");
    match send(HttpMethod::Post, &url, Some(&bytes), &[]).await {
        Ok(HttpJson { status, body }) if status < 300 => {
            match serde_json::from_str::<RedeemResponse>(&body) {
                Ok(response) => match response.into_result() {
                    Ok(result) => finish_redeem(offer, result).await,
                    Err(message) => Err(RedeemOutcome::Terminal { message }),
                },
                Err(err) => Err(RedeemOutcome::Auth(
                    MobileServiceAuthState::ServiceUnavailable {
                        message: format!(
                            "tycode.dev returned an unreadable redeem response: {err}"
                        ),
                        retryable: true,
                    },
                )),
            }
        }
        Ok(HttpJson { body, .. }) => Err(redeem_outcome_from_error(&body, &load_config())),
        Err(message) => Err(RedeemOutcome::Auth(
            MobileServiceAuthState::ServiceUnavailable {
                message,
                retryable: true,
            },
        )),
    }
}

/// Persists the durable managed pairing (host record + PSK + device secret),
/// caches the initial broker grant **in memory only**, and starts the managed
/// connection. Shared by the live and stubbed redeem paths.
async fn finish_redeem(
    offer: &ManagedMobilePairingQrPayload,
    result: RedeemResult,
) -> Result<(), RedeemOutcome> {
    let psk_store = IndexedDbPskStore;
    let psk_key_id = psk_store
        .store(&offer.psk)
        .await
        .map_err(|message| RedeemOutcome::Terminal { message })?;
    let device_secret_key_id =
        match super::store::store_device_secret(&result.device_pairing_secret).await {
            Ok(key_id) => key_id,
            Err(message) => {
                let _ = psk_store.delete(&psk_key_id).await;
                return Err(RedeemOutcome::Terminal { message });
            }
        };

    let broker_endpoint = mqtt_transport::BrokerEndpoint {
        url: result.broker.endpoint.clone(),
        auth: mqtt_transport::BrokerAuth::Anonymous,
    };
    let fingerprint =
        super::store::credential_fingerprint(&broker_endpoint, &offer.room, &offer.psk);
    let record = WebPairedHostRecord {
        local_host_id: LocalHostId(uuid::Uuid::new_v4().to_string()),
        host_label: offer.host_label.trim().to_owned(),
        broker: broker_endpoint,
        room: offer.room,
        psk_keychain_key_id: psk_key_id.clone(),
        credential_fingerprint: fingerprint,
        auto_connect: true,
        last_connected_at_ms: None,
        managed: Some(ManagedPairingRecord {
            pairing_id: result.pairing_id,
            device_id: result.device_id,
            broker: result.broker,
            device_secret_key_id: device_secret_key_id.clone(),
        }),
    };
    let local_host_id = record.local_host_id.clone();

    let host_store = IndexedDbHostStore;
    if let Err(message) = host_store.insert(record).await {
        let _ = psk_store.delete(&psk_key_id).await;
        let _ = super::store::delete_device_secret(&device_secret_key_id).await;
        return Err(RedeemOutcome::Terminal { message });
    }

    // Reuse the redeem-issued grant for the first connect (in memory only).
    if let Some(credentials) = result.mobile_broker_credentials {
        cache_credentials(&local_host_id, credentials);
    }

    if let Err(message) = super::connection::manager()
        .connect(local_host_id.clone())
        .await
    {
        // The record is stored; a failed initial connect is a transport concern
        // the connection manager surfaces and retries, not a redeem failure.
        log::warn!("managed pairing stored but initial connect failed: {message}");
    }
    super::emit_paired_hosts_changed().await;
    Ok(())
}

// ── Live HTTP: mint broker credentials (reconnect) ────────────────────────────

async fn mint_managed_credentials(
    record: &WebPairedHostRecord,
    managed: &ManagedPairingRecord,
) -> Result<(ManagedBrokerEndpoint, ManagedBrokerCredentials), ManagedCredentialError> {
    let config = load_config();
    let Some(base_url) = config.base_url.clone() else {
        return Err(ManagedCredentialError {
            code: MobileAccessErrorCode::ServiceUnavailable,
            message: "no tycode.dev endpoint is configured to refresh broker credentials"
                .to_owned(),
            retryable: false,
        });
    };
    // A reconnect that follows a "Sign in with Tyggs again" redirect returns
    // here with a one-time `handoffCode` still in the URL (the pairing
    // `authenticate` path never ran). Exchange it for the session cookie first
    // so the cookie-authenticated mint below can succeed instead of looping on
    // `mobile_session_required`.
    if let Some(handoff_code) = take_handoff_code() {
        let state = complete_handoff(&base_url, &config, &handoff_code).await;
        handoff_auth_state_to_credential_result(state)?;
    }
    let device_secret = super::store::load_device_secret(&managed.device_secret_key_id)
        .await
        .map_err(|message| ManagedCredentialError {
            code: MobileAccessErrorCode::RepairRequired,
            message,
            retryable: false,
        })?;

    let request = MintRequest {
        role: "mobile",
        client_instance_id: nonce(),
        protocol_version: PROTOCOL_VERSION,
        transport_protocol_version: MQTT_TRANSPORT_PROTOCOL_VERSION,
        requested_rooms: vec![RequestedRoom {
            room_id: record.room.to_string(),
            purpose: "rendezvous",
        }],
    };
    let body = serde_json::to_vec(&request).map_err(|err| ManagedCredentialError {
        code: MobileAccessErrorCode::Internal,
        message: format!("failed to serialize broker credential request: {err}"),
        retryable: false,
    })?;
    let endpoint_path = format!("/pairings/{}/broker-credentials", managed.pairing_id);
    let path = format!("{}{endpoint_path}", url_path_prefix(&base_url));
    let auth_header =
        pairing_auth_header(&device_secret, "POST", &path, &body).map_err(|message| {
            ManagedCredentialError {
                code: MobileAccessErrorCode::Internal,
                message,
                retryable: false,
            }
        })?;
    let url = format!("{base_url}{endpoint_path}");
    let headers = [(PAIRING_AUTH_HEADER, auth_header.as_str())];

    match send(HttpMethod::Post, &url, Some(&body), &headers).await {
        Ok(HttpJson { status, body }) if status < 300 => {
            let response: MintResponse =
                serde_json::from_str(&body).map_err(|err| ManagedCredentialError {
                    code: MobileAccessErrorCode::ServiceUnavailable,
                    message: format!("unreadable broker credential response: {err}"),
                    retryable: true,
                })?;
            let broker = response.broker.into_protocol().map_err(mint_conv_err)?;
            let credentials = response
                .broker_credentials
                .into_protocol(&broker)
                .map_err(mint_conv_err)?;
            // Cache in memory for the next reconnect within its lifetime; never
            // persisted (finding #5).
            cache_credentials(&record.local_host_id, credentials.clone());
            Ok((broker, credentials))
        }
        Ok(HttpJson { body, .. }) => Err(mint_error_from_body(&body)),
        Err(message) => Err(ManagedCredentialError {
            code: MobileAccessErrorCode::ServiceUnavailable,
            message,
            retryable: true,
        }),
    }
}

// ── HMAC (mirrors server::mobile_access::pairing_auth_header) ─────────────────

fn pairing_auth_header(
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<String, String> {
    if secret.trim().is_empty() {
        return Err("managed mobile device pairing secret is missing".to_owned());
    }
    let nonce = nonce();
    let timestamp_ms = now_ms();
    let body_sha256 = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
    // Canonical string is newline-joined, byte-identical to the host signer:
    // PREFIX\nMETHOD\nPATH\nBODY_SHA256\nNONCE\nTIMESTAMP_MS\nPAIRING_ID\nROLE.
    // The pairing id is embedded in `path`; extract it so the tail matches.
    let pairing_id = pairing_id_from_path(path);
    let mut canonical = Vec::new();
    let mut push = |field: &str| {
        canonical.extend_from_slice(field.as_bytes());
        canonical.push(b'\n');
    };
    push(PAIRING_HMAC_PREFIX);
    push(method);
    push(path);
    push(&body_sha256);
    push(&nonce);
    push(&timestamp_ms.to_string());
    push(pairing_id);
    canonical.extend_from_slice(b"mobile");
    let signature = URL_SAFE_NO_PAD.encode(hmac_sha256(secret.as_bytes(), &canonical));
    Ok(format!(
        "v1;role=mobile;nonce={nonce};timestamp_ms={timestamp_ms};signature={signature}"
    ))
}

/// Extracts the `{pairing_id}` segment from a `.../pairings/{id}/broker-credentials`
/// path so the HMAC canonical string binds the pairing exactly as the server does.
fn pairing_id_from_path(path: &str) -> &str {
    path.rsplit_once("/pairings/")
        .and_then(|(_, tail)| tail.split('/').next())
        .unwrap_or("")
}

/// HMAC-SHA256 without pulling a separate `hmac` crate (which would force a
/// second `sha2`/`digest` version into the wasm bundle).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        ipad[index] ^= block[index];
        opad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

// ── HTTP plumbing ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }
}

struct HttpJson {
    status: u16,
    body: String,
}

/// Issues a `tycode.dev` request with the session cookie attached
/// (`credentials: "include"`). No auth secret is ever taken from a JS global or
/// added as a bearer header — the cookie is the sole session credential.
async fn send(
    method: HttpMethod,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
) -> Result<HttpJson, String> {
    let window = web_sys::window().ok_or("no window for fetch")?;
    let init = web_sys::RequestInit::new();
    init.set_method(method.as_str());
    init.set_mode(web_sys::RequestMode::Cors);
    init.set_credentials(web_sys::RequestCredentials::Include);

    let header_map = web_sys::Headers::new().map_err(js_err)?;
    if let Some(body) = body {
        let body_string = String::from_utf8(body.to_vec())
            .map_err(|err| format!("request body was not valid UTF-8: {err}"))?;
        init.set_body(&JsValue::from_str(&body_string));
        header_map
            .append("content-type", "application/json")
            .map_err(js_err)?;
    }
    for (name, value) in headers {
        header_map.append(name, value).map_err(js_err)?;
    }
    init.set_headers(&header_map);

    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let response_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|err| format!("tycode.dev request failed: {}", js_err(err)))?;
    let response: web_sys::Response = response_value
        .dyn_into()
        .map_err(|_| "fetch did not return a Response".to_owned())?;
    let status = response.status();
    let text_value = JsFuture::from(response.text().map_err(js_err)?)
        .await
        .map_err(js_err)?;
    let body = text_value.as_string().unwrap_or_default();
    Ok(HttpJson { status, body })
}

fn js_err(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

fn nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn url_path_prefix(base_url: &str) -> String {
    match web_sys::Url::new(base_url) {
        Ok(url) => url.pathname().trim_end_matches('/').to_owned(),
        Err(_) => String::new(),
    }
}

fn credentials_are_fresh(credentials: &ManagedBrokerCredentials, now_ms: u64) -> bool {
    credentials.expires_at_ms > now_ms.saturating_add(CREDENTIAL_REFRESH_MARGIN_MS)
}

// ── Error mapping ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
    #[serde(default)]
    retryable: Option<bool>,
    #[serde(default)]
    paywall_url: Option<String>,
}

fn parse_error(body: &str) -> Option<ErrorBody> {
    serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .map(|envelope| envelope.error)
}

fn handoff_auth_state_to_credential_result(
    state: MobileServiceAuthState,
) -> Result<(), ManagedCredentialError> {
    match state {
        MobileServiceAuthState::Authenticated { .. } => Ok(()),
        MobileServiceAuthState::PassRequired { message, .. } => Err(ManagedCredentialError {
            code: MobileAccessErrorCode::PassRequired,
            message,
            retryable: false,
        }),
        MobileServiceAuthState::AuthFailed { message } => Err(ManagedCredentialError {
            code: MobileAccessErrorCode::ServiceAuthRequired,
            message,
            retryable: false,
        }),
        MobileServiceAuthState::ServiceUnavailable { message, retryable } => {
            Err(ManagedCredentialError {
                code: MobileAccessErrorCode::ServiceUnavailable,
                message,
                retryable,
            })
        }
        MobileServiceAuthState::Idle | MobileServiceAuthState::Authenticating => {
            Err(ManagedCredentialError {
                code: MobileAccessErrorCode::Internal,
                message: "managed auth handoff did not produce a terminal state".to_owned(),
                retryable: false,
            })
        }
    }
}

fn auth_state_from_error(body: &str, config: &ServiceConfig) -> MobileServiceAuthState {
    let Some(error) = parse_error(body) else {
        return MobileServiceAuthState::ServiceUnavailable {
            message: "tycode.dev returned an unexpected error.".to_owned(),
            retryable: true,
        };
    };
    // Error `code` strings are exactly the documented set in
    // dev-docs/30 "Common error codes" (no undocumented codes like
    // `unauthenticated`). Unknown codes fall through and are surfaced, never
    // silently swallowed.
    match error.code.as_str() {
        "pass_required" => MobileServiceAuthState::PassRequired {
            message: error.message,
            paywall_url: error.paywall_url.unwrap_or_else(|| paywall_url(config)),
        },
        // 401 "not signed in" states drive the Tyggs sign-in redirect, not a retry.
        "invalid_tyggs_auth" | "mobile_session_required" => sign_in_required(),
        // 400/403 are terminal client/permission errors on the auth probe: surface
        // the service message rather than looping on a retry.
        "invalid_request" | "forbidden" => MobileServiceAuthState::AuthFailed {
            message: error.message,
        },
        // 503/429/500 families are transient — offer a retry.
        "service_unavailable" | "broker_unavailable" | "rate_limited" | "internal" => {
            MobileServiceAuthState::ServiceUnavailable {
                message: error.message,
                retryable: true,
            }
        }
        _ => MobileServiceAuthState::ServiceUnavailable {
            message: error.message,
            retryable: error.retryable.unwrap_or(true),
        },
    }
}

fn redeem_outcome_from_error(body: &str, config: &ServiceConfig) -> RedeemOutcome {
    let Some(error) = parse_error(body) else {
        return RedeemOutcome::Auth(MobileServiceAuthState::ServiceUnavailable {
            message: "tycode.dev returned an unexpected error.".to_owned(),
            retryable: true,
        });
    };
    // Documented redeem failure codes (dev-docs/30): pass_required, the two
    // 401 session codes, and the terminal offer/pairing states. No
    // undocumented codes.
    match error.code.as_str() {
        "pass_required" => RedeemOutcome::Auth(MobileServiceAuthState::PassRequired {
            message: error.message,
            paywall_url: error.paywall_url.unwrap_or_else(|| paywall_url(config)),
        }),
        "invalid_tyggs_auth" | "mobile_session_required" => {
            mark_authenticated(false);
            RedeemOutcome::Auth(sign_in_required())
        }
        // Terminal, re-pair-actionable states: the offer/pairing can't be
        // redeemed as-is, or the caller/bundle is no longer compatible.
        "repair_required"
        | "offer_expired"
        | "offer_already_redeemed"
        | "not_found"
        | "duplicate_device"
        | "forbidden"
        | "version_mismatch" => RedeemOutcome::Repair {
            message: error.message,
        },
        // A malformed request is a client bug, not something the user can retry.
        "invalid_request" => RedeemOutcome::Terminal {
            message: error.message,
        },
        "service_unavailable" | "broker_unavailable" | "rate_limited" | "internal" => {
            RedeemOutcome::Auth(MobileServiceAuthState::ServiceUnavailable {
                message: error.message,
                retryable: true,
            })
        }
        _ => RedeemOutcome::Auth(MobileServiceAuthState::ServiceUnavailable {
            message: error.message,
            retryable: error.retryable.unwrap_or(true),
        }),
    }
}

fn mint_error_from_body(body: &str) -> ManagedCredentialError {
    let Some(error) = parse_error(body) else {
        return ManagedCredentialError {
            code: MobileAccessErrorCode::ServiceUnavailable,
            message: "tycode.dev returned an unexpected error.".to_owned(),
            retryable: true,
        };
    };
    // Documented broker-credential failure states (dev-docs/30 §broker-credentials
    // + Common error codes). No undocumented codes.
    let (code, retryable) = match error.code.as_str() {
        "pass_required" => (MobileAccessErrorCode::PassRequired, false),
        "invalid_tyggs_auth" | "mobile_session_required" => {
            mark_authenticated(false);
            (MobileAccessErrorCode::ServiceAuthRequired, false)
        }
        "repair_required" | "pairing_revoked" | "version_mismatch" | "not_found" | "forbidden" => {
            (MobileAccessErrorCode::RepairRequired, false)
        }
        "broker_unavailable" => (MobileAccessErrorCode::BrokerUnavailable, true),
        "service_unavailable" | "rate_limited" | "internal" => {
            (MobileAccessErrorCode::ServiceUnavailable, true)
        }
        // A malformed request is a client bug, not a transient failure.
        "invalid_request" => (MobileAccessErrorCode::Internal, false),
        _ => (
            MobileAccessErrorCode::ServiceUnavailable,
            error.retryable.unwrap_or(true),
        ),
    };
    ManagedCredentialError {
        code,
        message: error.message,
        retryable,
    }
}

fn mint_conv_err(message: String) -> ManagedCredentialError {
    ManagedCredentialError {
        code: MobileAccessErrorCode::InvalidConfig,
        message,
        retryable: false,
    }
}

// ── Contract types (mobile client's view of the tycode.dev JSON) ──────────────
//
// Structs that carry any secret (device pairing secret, offer secret, broker
// password/headers/grant token) deliberately do NOT derive `Debug`, so they can
// never be formatted into a log line (finding #6).

#[derive(Deserialize)]
struct SessionResponse {
    authenticated: bool,
    #[serde(default)]
    expires_at_ms: Option<u64>,
}

/// Body of `POST /auth/session`: the one-time handoff marker plus this build's
/// client identity. Carries no Tyggs secret — the session is completed
/// server-side and returned as an `HttpOnly` cookie.
#[derive(Serialize)]
struct AuthSessionRequest {
    handoff_code: String,
    client: AuthClient,
}

#[derive(Serialize)]
struct AuthClient {
    kind: &'static str,
    release_version: String,
    protocol_version: u32,
}

#[derive(Serialize)]
struct RedeemRequest {
    offer_id: String,
    offer_secret: String,
    device_label: String,
    device_nonce: String,
    release_version: String,
    protocol_version: u32,
    transport_protocol_version: u32,
}

#[derive(Debug, Serialize)]
struct MintRequest {
    role: &'static str,
    client_instance_id: String,
    protocol_version: u32,
    transport_protocol_version: u32,
    requested_rooms: Vec<RequestedRoom>,
}

#[derive(Debug, Serialize)]
struct RequestedRoom {
    room_id: String,
    purpose: &'static str,
}

/// Parsed redeem result in mobile-side types. Built from the live JSON response
/// or the dev stub; consumed by [`finish_redeem`]. No `Debug` — holds the device
/// pairing secret.
struct RedeemResult {
    pairing_id: String,
    device_id: String,
    device_pairing_secret: String,
    broker: ManagedBrokerEndpoint,
    mobile_broker_credentials: Option<ManagedBrokerCredentials>,
}

#[derive(Deserialize)]
struct RedeemResponse {
    pairing_id: String,
    device_id: String,
    device_pairing_secret: String,
    broker: ContractBroker,
    mobile_broker_credentials: ContractCredentials,
}

impl RedeemResponse {
    fn into_result(self) -> Result<RedeemResult, String> {
        let broker = self.broker.into_protocol()?;
        let credentials = self.mobile_broker_credentials.into_protocol(&broker)?;
        Ok(RedeemResult {
            pairing_id: self.pairing_id,
            device_id: self.device_id,
            device_pairing_secret: self.device_pairing_secret,
            broker,
            mobile_broker_credentials: Some(credentials),
        })
    }
}

#[derive(Deserialize)]
struct MintResponse {
    broker: ContractBroker,
    broker_credentials: ContractCredentials,
}

#[derive(Debug, Deserialize)]
struct ContractBroker {
    endpoint: String,
    provider: String,
    region: String,
    authorizer_name: String,
}

impl ContractBroker {
    fn into_protocol(self) -> Result<ManagedBrokerEndpoint, String> {
        let provider = match self.provider.as_str() {
            "aws_iot_core" => ManagedBrokerProvider::AwsIotCore,
            other => return Err(format!("unsupported managed broker provider {other:?}")),
        };
        Ok(ManagedBrokerEndpoint {
            endpoint: protocol::BrokerUrl::new(&self.endpoint)
                .map_err(|err| format!("invalid managed broker endpoint: {err}"))?,
            provider,
            region: ManagedBrokerRegion::new(self.region)
                .map_err(|err| format!("invalid managed broker region: {err}"))?,
            authorizer_name: ManagedBrokerAuthorizerName::new(self.authorizer_name)
                .map_err(|err| format!("invalid managed broker authorizer: {err}"))?,
        })
    }
}

#[derive(Deserialize)]
struct ContractCredentials {
    grant_id: String,
    client_id: String,
    connect: ContractConnect,
    scope: ContractScope,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

impl ContractCredentials {
    fn into_protocol(
        self,
        broker: &ManagedBrokerEndpoint,
    ) -> Result<ManagedBrokerCredentials, String> {
        Ok(ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new(self.grant_id)
                .map_err(|err| format!("invalid managed grant id: {err}"))?,
            client_id: ManagedBrokerClientId::new(self.client_id)
                .map_err(|err| format!("invalid managed client id: {err}"))?,
            connect: self.connect.into_protocol(broker)?,
            scope: self.scope.into_protocol()?,
            issued_at_ms: self.issued_at_ms,
            expires_at_ms: self.expires_at_ms,
        })
    }
}

#[derive(Deserialize)]
struct ContractConnect {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    websocket_url: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

impl ContractConnect {
    fn into_protocol(
        self,
        broker: &ManagedBrokerEndpoint,
    ) -> Result<ManagedBrokerConnectAuth, String> {
        let websocket_url = validate_managed_websocket_url(self.websocket_url, broker)?;
        Ok(ManagedBrokerConnectAuth {
            username: self.username,
            password: self.password,
            websocket_url: Some(websocket_url),
            headers: self.headers,
        })
    }
}

fn validate_managed_websocket_url(
    websocket_url: Option<String>,
    broker: &ManagedBrokerEndpoint,
) -> Result<protocol::BrokerUrl, String> {
    let websocket_url = websocket_url.ok_or_else(|| {
        "managed AWS IoT browser credentials require connect.websocket_url".to_owned()
    })?;
    if websocket_url.trim() != websocket_url || websocket_url.is_empty() {
        return Err("managed broker connect.websocket_url must not be empty or padded".to_owned());
    }
    if websocket_url
        .bytes()
        .any(|byte| byte <= 0x20 || byte == 0x7f)
    {
        return Err(
            "managed broker connect.websocket_url must not contain control or whitespace characters"
                .to_owned(),
        );
    }
    if websocket_url.contains('#') {
        return Err("managed broker connect.websocket_url must not contain a fragment".to_owned());
    }

    let base_endpoint = broker.endpoint.as_str();
    validate_managed_broker_endpoint_base(base_endpoint)?;
    let (url_base, query) = websocket_url.split_once('?').ok_or_else(|| {
        "managed broker connect.websocket_url must include AWS IoT custom-authorizer query parameters"
            .to_owned()
    })?;
    if url_base != base_endpoint {
        return Err(
            "managed broker connect.websocket_url base must match broker endpoint".to_owned(),
        );
    }
    if query.is_empty() {
        return Err(
            "managed broker connect.websocket_url custom-authorizer query must not be empty"
                .to_owned(),
        );
    }

    let authorizer = query_value(query, "x-amz-customauthorizer-name").ok_or_else(|| {
        "managed broker connect.websocket_url is missing x-amz-customauthorizer-name".to_owned()
    })?;
    if authorizer != broker.authorizer_name.as_str() {
        return Err(format!(
            "managed broker connect.websocket_url authorizer {authorizer:?} does not match broker authorizer {:?}",
            broker.authorizer_name.as_str()
        ));
    }
    if let Some(token_key) = query_value(query, "token-key-name")
        && token_key != "tycode-grant"
    {
        return Err(format!(
            "managed broker connect.websocket_url token-key-name {token_key:?} is unsupported; expected \"tycode-grant\""
        ));
    }
    let token = query_value(query, "tycode-grant").ok_or_else(|| {
        "managed broker connect.websocket_url is missing custom-authorizer token parameter \"tycode-grant\""
            .to_owned()
    })?;
    if token.trim().is_empty() {
        return Err(
            "managed broker connect.websocket_url custom-authorizer token must not be empty"
                .to_owned(),
        );
    }

    protocol::BrokerUrl::new(websocket_url)
        .map_err(|err| format!("invalid managed broker connect.websocket_url: {err}"))
}

fn validate_managed_broker_endpoint_base(base_endpoint: &str) -> Result<(), String> {
    if !base_endpoint.starts_with("wss://") {
        return Err(
            "managed AWS IoT browser credentials require a wss:// broker endpoint".to_owned(),
        );
    }
    if base_endpoint.contains('?') {
        return Err(
            "managed broker endpoint must not include query parameters when validating connect.websocket_url"
                .to_owned(),
        );
    }
    if base_endpoint.contains('#') {
        return Err(
            "managed broker endpoint must not include fragments when validating connect.websocket_url"
                .to_owned(),
        );
    }
    let without_scheme = base_endpoint.trim_start_matches("wss://");
    let (host, path) = without_scheme.split_once('/').ok_or_else(|| {
        "managed AWS IoT browser credentials require a /mqtt broker endpoint path".to_owned()
    })?;
    if host.is_empty() {
        return Err("managed broker endpoint is missing a host".to_owned());
    }
    if host.contains('@') {
        return Err(
            "managed broker endpoint must not embed URL username/password credentials".to_owned(),
        );
    }
    if path != "mqtt" {
        return Err(format!(
            "managed AWS IoT browser credentials require broker endpoint path /mqtt; got /{path}"
        ));
    }
    Ok(())
}

fn query_value(query: &str, wanted: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode_query_component(key)?;
        if key == wanted {
            Some(percent_decode_query_component(value)?)
        } else {
            None
        }
    })
}

fn percent_decode_query_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1)?;
                let low = *bytes.get(index + 2)?;
                output.push((hex_value(high)? << 4) | hex_value(low)?);
                index += 3;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct ContractScope {
    namespace: String,
    role: String,
    publish: Vec<String>,
    subscribe: Vec<String>,
}

impl ContractScope {
    fn into_protocol(self) -> Result<ManagedBrokerCredentialScope, String> {
        let role = match self.role.as_str() {
            "host" => ManagedBrokerRole::Host,
            "mobile" => ManagedBrokerRole::Mobile,
            other => return Err(format!("unsupported managed broker role {other:?}")),
        };
        Ok(ManagedBrokerCredentialScope {
            namespace: ManagedBrokerTopicNamespace::new(self.namespace)
                .map_err(|err| format!("invalid managed topic namespace: {err}"))?,
            role,
            publish: self.publish,
            subscribe: self.subscribe,
        })
    }
}

// ── Managed offer parsing ─────────────────────────────────────────────────────

/// Parses `qr_uri` as a managed (`tyde-pair://v2`) offer, accepting both the raw
/// URI and the `https://tycode.dev/tyde/#…` fragment-wrapped loader form. The
/// extended v2 payload carries `offer_id`, `offer_secret`, `broker`, `room`, and
/// `psk` — everything the redeem + connect needs.
fn parse_managed_offer(qr_uri: &str) -> Result<ManagedMobilePairingQrPayload, String> {
    ManagedMobilePairingQrPayload::from_any(qr_uri)
        .map_err(|error| format!("not a managed Tyde pairing offer: {error}"))
}

// ── Dev/test stubs ────────────────────────────────────────────────────────────

fn stub_auth_state(kind: &str, config: &ServiceConfig) -> MobileServiceAuthState {
    match kind {
        "authenticated" => {
            mark_authenticated(true);
            MobileServiceAuthState::Authenticated {
                expires_at_ms: now_ms().saturating_add(3_600_000),
            }
        }
        "pass_required" => {
            mark_authenticated(false);
            MobileServiceAuthState::PassRequired {
                message: "A Tyggs Pass is required for Tyde mobile access.".to_owned(),
                paywall_url: paywall_url(config),
            }
        }
        "auth_failed" => {
            mark_authenticated(false);
            MobileServiceAuthState::AuthFailed {
                message: "Sign in with your Tyggs account to continue.".to_owned(),
            }
        }
        "service_unavailable" => MobileServiceAuthState::ServiceUnavailable {
            message: "tycode.dev is temporarily unavailable. Try again in a moment.".to_owned(),
            retryable: true,
        },
        other => MobileServiceAuthState::ServiceUnavailable {
            message: format!("Unknown managed-service stub outcome {other:?}."),
            retryable: false,
        },
    }
}

/// Synthesizes a redeem result for the `stubRedeem: "ok"` dev/test path so the
/// persistence + connect wiring is exercisable without a live service. No broker
/// credentials are minted here (the fake broker can't be reached); the stored
/// record is what the tests assert.
fn stub_redeem_result(offer: &ManagedMobilePairingQrPayload) -> RedeemResult {
    RedeemResult {
        pairing_id: format!("pair_stub_{}", offer.offer_id.as_str()),
        device_id: "dev_stub".to_owned(),
        device_pairing_secret: "device_pairing_secret_stub".to_owned(),
        broker: offer.broker.clone(),
        mobile_broker_credentials: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_vector() {
        // RFC 4231 Test Case 2: key="Jefe", data="what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pairing_id_is_extracted_from_path() {
        assert_eq!(
            pairing_id_from_path("/api/tyde/mobile/v1/pairings/pair_01J/broker-credentials"),
            "pair_01J"
        );
        assert_eq!(pairing_id_from_path("/no-pairing-segment"), "");
    }

    fn error_body(code: &str) -> String {
        format!(r#"{{"error":{{"code":"{code}","message":"m"}}}}"#)
    }

    #[test]
    fn auth_error_maps_documented_codes() {
        let cfg = ServiceConfig::default();
        // 401 session codes drive the Tyggs sign-in redirect (AuthFailed).
        for code in ["invalid_tyggs_auth", "mobile_session_required"] {
            assert!(
                matches!(
                    auth_state_from_error(&error_body(code), &cfg),
                    MobileServiceAuthState::AuthFailed { .. }
                ),
                "{code} must map to sign-in-required"
            );
        }
        // 402 → paywall, echoing the service-provided URL.
        let body = r#"{"error":{"code":"pass_required","message":"m","paywall_url":"https://p"}}"#;
        match auth_state_from_error(body, &cfg) {
            MobileServiceAuthState::PassRequired { paywall_url, .. } => {
                assert_eq!(paywall_url, "https://p")
            }
            other => panic!("expected pass_required, got {other:?}"),
        }
        // 503/429/500 families are retryable.
        for code in [
            "service_unavailable",
            "broker_unavailable",
            "rate_limited",
            "internal",
        ] {
            assert!(
                matches!(
                    auth_state_from_error(&error_body(code), &cfg),
                    MobileServiceAuthState::ServiceUnavailable {
                        retryable: true,
                        ..
                    }
                ),
                "{code} must be a retryable service error"
            );
        }
    }

    #[test]
    fn redeem_error_maps_documented_codes() {
        let cfg = ServiceConfig::default();
        for code in [
            "repair_required",
            "offer_expired",
            "offer_already_redeemed",
            "not_found",
            "duplicate_device",
            "forbidden",
            "version_mismatch",
        ] {
            assert!(
                matches!(
                    redeem_outcome_from_error(&error_body(code), &cfg),
                    RedeemOutcome::Repair { .. }
                ),
                "{code} must be a terminal re-pair state"
            );
        }
        for code in ["invalid_tyggs_auth", "mobile_session_required"] {
            assert!(
                matches!(
                    redeem_outcome_from_error(&error_body(code), &cfg),
                    RedeemOutcome::Auth(MobileServiceAuthState::AuthFailed { .. })
                ),
                "{code} must ask the user to sign in"
            );
        }
        assert!(matches!(
            redeem_outcome_from_error(&error_body("invalid_request"), &cfg),
            RedeemOutcome::Terminal { .. }
        ));
    }

    #[test]
    fn mint_error_maps_documented_codes() {
        for code in [
            "repair_required",
            "pairing_revoked",
            "version_mismatch",
            "not_found",
            "forbidden",
        ] {
            let error = mint_error_from_body(&error_body(code));
            assert_eq!(
                error.code,
                MobileAccessErrorCode::RepairRequired,
                "{code} must require repair"
            );
            assert!(!error.retryable, "{code} is terminal");
        }
        let error = mint_error_from_body(&error_body("broker_unavailable"));
        assert_eq!(error.code, MobileAccessErrorCode::BrokerUnavailable);
        assert!(error.retryable);
        let error = mint_error_from_body(&error_body("invalid_tyggs_auth"));
        assert_eq!(error.code, MobileAccessErrorCode::ServiceAuthRequired);
        assert!(!error.retryable);
    }

    /// An *undocumented* code (like the removed `unauthenticated`) must fall
    /// through to a surfaced service error — never be silently special-cased as
    /// a sign-in or repair state. Locks in the "documented codes only" invariant.
    #[test]
    fn undocumented_code_is_surfaced_not_special_cased() {
        let cfg = ServiceConfig::default();
        let body = error_body("unauthenticated");
        assert!(
            matches!(
                auth_state_from_error(&body, &cfg),
                MobileServiceAuthState::ServiceUnavailable { .. }
            ),
            "an undocumented auth code must not be treated as sign-in-required"
        );
        assert!(matches!(
            redeem_outcome_from_error(&body, &cfg),
            RedeemOutcome::Auth(MobileServiceAuthState::ServiceUnavailable { .. })
        ));
        assert_eq!(
            mint_error_from_body(&body).code,
            MobileAccessErrorCode::ServiceUnavailable
        );
    }

    #[test]
    fn credentials_are_fresh_respects_margin() {
        let creds = sample_credentials(10_000);
        assert!(
            !credentials_are_fresh(&creds, 0),
            "expiry within margin is stale"
        );
        let creds = sample_credentials(1_000_000);
        assert!(
            credentials_are_fresh(&creds, 100),
            "distant expiry is fresh"
        );
    }

    #[test]
    fn service_contract_preserves_managed_websocket_url() {
        let broker = sample_contract_broker();
        let credentials = sample_contract_credentials(Some(sample_websocket_url()))
            .into_protocol(&broker)
            .expect("credential contract");
        assert_eq!(
            credentials
                .connect
                .websocket_url
                .as_ref()
                .map(|url| url.as_str()),
            Some(sample_websocket_url())
        );
    }

    #[test]
    fn service_contract_accepts_current_tycode_grant_query() {
        let broker = sample_contract_broker();
        let credentials = sample_contract_credentials(Some(
            "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant",
        ))
        .into_protocol(&broker)
        .expect("credential contract without token-key-name");
        assert_eq!(
            credentials
                .connect
                .websocket_url
                .as_ref()
                .map(|url| url.as_str()),
            Some(
                "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant"
            )
        );
    }

    #[test]
    fn service_contract_rejects_missing_or_invalid_websocket_url() {
        let broker = sample_contract_broker();
        for (websocket_url, expected) in [
            (None, "connect.websocket_url"),
            (
                Some(
                    "wss://other-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant",
                ),
                "must match broker endpoint",
            ),
            (
                Some(
                    "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=other&token-key-name=tycode-grant&tycode-grant=signed-grant",
                ),
                "authorizer",
            ),
            (
                Some(
                    "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/not-mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant",
                ),
                "must match broker endpoint",
            ),
            (
                Some(
                    "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=other&other=signed-grant",
                ),
                "token-key-name",
            ),
            (
                Some(
                    "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant",
                ),
                "tycode-grant",
            ),
        ] {
            let error = sample_contract_credentials(websocket_url)
                .into_protocol(&broker)
                .expect_err("invalid websocket_url must fail closed");
            assert!(
                error.contains(expected),
                "expected {expected:?} in {error:?}"
            );
            assert!(
                !error.contains("signed-grant"),
                "managed websocket_url errors must not leak grant tokens: {error}"
            );
        }
    }

    fn sample_contract_broker() -> ManagedBrokerEndpoint {
        ContractBroker {
            endpoint: "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt".to_owned(),
            provider: "aws_iot_core".to_owned(),
            region: "us-west-2".to_owned(),
            authorizer_name: "tycode-mobile-v1".to_owned(),
        }
        .into_protocol()
        .expect("broker")
    }

    fn sample_contract_credentials(websocket_url: Option<&str>) -> ContractCredentials {
        ContractCredentials {
            grant_id: "grant_1".to_owned(),
            client_id: "tyde/prod/pair_1/mobile/dev_1/grant_1".to_owned(),
            connect: ContractConnect {
                username: Some(
                    "x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant"
                        .to_owned(),
                ),
                password: Some("signed-grant".to_owned()),
                websocket_url: websocket_url.map(ToOwned::to_owned),
                headers: std::collections::BTreeMap::new(),
            },
            scope: ContractScope {
                namespace: "tyde/prod/pair_1".to_owned(),
                role: "mobile".to_owned(),
                publish: vec!["tyde/prod/pair_1/rooms/+/client-to-host".to_owned()],
                subscribe: vec!["tyde/prod/pair_1/rooms/+/host-to-client".to_owned()],
            },
            issued_at_ms: 0,
            expires_at_ms: 1,
        }
    }

    fn sample_websocket_url() -> &'static str {
        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant"
    }

    fn sample_credentials(expires_at_ms: u64) -> ManagedBrokerCredentials {
        ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new("grant_1").unwrap(),
            client_id: ManagedBrokerClientId::new("tyde/prod/pair_1/mobile/dev_1/grant_1").unwrap(),
            connect: ManagedBrokerConnectAuth {
                username: Some("u".to_owned()),
                password: Some("p".to_owned()),
                websocket_url: Some(
                    protocol::BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant",
                    )
                    .unwrap(),
                ),
                headers: Default::default(),
            },
            scope: ManagedBrokerCredentialScope {
                namespace: ManagedBrokerTopicNamespace::new("tyde/prod/pair_1").unwrap(),
                role: ManagedBrokerRole::Mobile,
                publish: vec!["tyde/prod/pair_1/rooms/+/client-to-host".to_owned()],
                subscribe: vec!["tyde/prod/pair_1/rooms/+/host-to-client".to_owned()],
            },
            issued_at_ms: 0,
            expires_at_ms,
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn set_config(json: &str) {
        let window = web_sys::window().expect("window");
        if json.is_empty() {
            let _ = js_sys::Reflect::delete_property(&window, &JsValue::from_str(CONFIG_GLOBAL));
            mark_authenticated(false);
            return;
        }
        let value = js_sys::JSON::parse(json).expect("parse config json");
        js_sys::Reflect::set(&window, &JsValue::from_str(CONFIG_GLOBAL), &value)
            .expect("install config");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FetchCall {
        method: String,
        url: String,
    }

    struct FetchMock {
        calls: Rc<RefCell<Vec<FetchCall>>>,
        original_fetch: JsValue,
        _closure: Closure<dyn FnMut(JsValue) -> js_sys::Promise>,
    }

    impl FetchMock {
        fn calls(&self) -> Vec<FetchCall> {
            self.calls.borrow().clone()
        }
    }

    impl Drop for FetchMock {
        fn drop(&mut self) {
            let window = web_sys::window().expect("window");
            js_sys::Reflect::set(&window, &JsValue::from_str("fetch"), &self.original_fetch)
                .expect("restore fetch");
        }
    }

    fn install_fetch_mock(responses: Vec<(u16, &'static str)>) -> FetchMock {
        let window = web_sys::window().expect("window");
        let original_fetch = js_sys::Reflect::get(&window, &JsValue::from_str("fetch"))
            .expect("read original fetch");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_for_fetch = calls.clone();
        let response_queue = Rc::new(RefCell::new(VecDeque::from(responses)));
        let responses_for_fetch = response_queue.clone();
        let closure = Closure::<dyn FnMut(JsValue) -> js_sys::Promise>::new(
            move |request_value: JsValue| {
                let request: web_sys::Request = request_value
                    .dyn_into()
                    .expect("managed-service client must call fetch with Request");
                calls_for_fetch.borrow_mut().push(FetchCall {
                    method: request.method(),
                    url: request.url(),
                });
                let (status, body) = responses_for_fetch
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or((500, r#"{"error":{"code":"internal","message":"unexpected extra request","retryable":true}}"#));
                response_promise(status, body)
            },
        );
        js_sys::Reflect::set(&window, &JsValue::from_str("fetch"), closure.as_ref())
            .expect("install fetch mock");
        FetchMock {
            calls,
            original_fetch,
            _closure: closure,
        }
    }

    fn response_promise(status: u16, body: &str) -> js_sys::Promise {
        let response = js_sys::Function::new_with_args(
            "body,status",
            "return new Response(body, { status });",
        )
        .call2(
            &JsValue::NULL,
            &JsValue::from_str(body),
            &JsValue::from_f64(f64::from(status)),
        )
        .expect("create Response");
        js_sys::Promise::resolve(&response)
    }

    fn reconnect_record(
        local_host_id: &str,
        device_secret_key_id: mobile_shell_types::KeychainSecretId,
    ) -> WebPairedHostRecord {
        let managed_broker = super::super::tests_support::sample_managed_broker();
        WebPairedHostRecord {
            local_host_id: LocalHostId(local_host_id.to_owned()),
            host_label: "Living Room".to_owned(),
            broker: mqtt_transport::BrokerEndpoint {
                url: managed_broker.endpoint.clone(),
                auth: mqtt_transport::BrokerAuth::Anonymous,
            },
            room: mqtt_transport::RoomId([7_u8; 16]),
            psk_keychain_key_id: mobile_shell_types::KeychainSecretId("psk-test".to_owned()),
            credential_fingerprint: "fingerprint".to_owned(),
            auto_connect: true,
            last_connected_at_ms: None,
            managed: Some(ManagedPairingRecord {
                pairing_id: "pair_01J".to_owned(),
                device_id: "dev_01J".to_owned(),
                broker: managed_broker,
                device_secret_key_id,
            }),
        }
    }

    #[wasm_bindgen_test]
    async fn authenticate_without_config_fails_closed_non_retryable() {
        set_config("{}");
        let uri = super::super::tests_support::sample_managed_uri();
        let state = authenticate(&uri).await;
        assert!(
            matches!(
                state,
                MobileServiceAuthState::ServiceUnavailable {
                    retryable: false,
                    ..
                }
            ),
            "unconfigured managed auth must fail closed: {state:?}"
        );
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn no_js_global_tyggs_secret_is_read() {
        // Even if a caller stuffs Tyggs secrets into the config global, the
        // client must ignore them (they must never be JS-global auth material).
        set_config(
            r#"{"tyggsOauthAccessToken":"leak","tyggsPassProof":"leak","stubAuth":"pass_required"}"#,
        );
        let uri = super::super::tests_support::sample_managed_uri();
        // Falls through to the stub (no baseUrl) and never uses the injected
        // secrets; a real build would GET /auth/session with the cookie instead.
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::PassRequired { .. }
        ));
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn malformed_offer_is_auth_failed() {
        set_config("{}");
        let state = authenticate("tyde-pair://v2?not-a-real-offer").await;
        assert!(matches!(state, MobileServiceAuthState::AuthFailed { .. }));
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn stub_drives_pass_required_with_paywall() {
        set_config(r#"{"stubAuth":"pass_required","paywallUrl":"https://tyggs.com/go"}"#);
        let uri = super::super::tests_support::sample_managed_uri();
        match authenticate(&uri).await {
            MobileServiceAuthState::PassRequired { paywall_url, .. } => {
                assert_eq!(paywall_url, "https://tyggs.com/go");
            }
            other => panic!("expected pass_required, got {other:?}"),
        }
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn redeem_before_sign_in_requires_auth() {
        set_config(r#"{"stubRedeem":"ok"}"#);
        let uri = super::super::tests_support::sample_managed_uri();
        // Not authenticated yet: even a would-be "ok" redeem must not proceed.
        match redeem_and_connect(&uri).await {
            Err(RedeemOutcome::Auth(MobileServiceAuthState::AuthFailed { .. })) => {}
            other => panic!("expected auth-required, got {other:?}"),
        }
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn stub_redeem_service_unavailable_is_retryable_auth() {
        set_config(r#"{"stubAuth":"authenticated","stubRedeem":"service_unavailable"}"#);
        let uri = super::super::tests_support::sample_managed_uri();
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::Authenticated { .. }
        ));
        match redeem_and_connect(&uri).await {
            Err(RedeemOutcome::Auth(MobileServiceAuthState::ServiceUnavailable {
                retryable,
                ..
            })) => assert!(retryable),
            other => panic!("expected retryable service_unavailable, got {other:?}"),
        }
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn stub_redeem_repair_required_is_terminal_repair() {
        set_config(r#"{"stubAuth":"authenticated","stubRedeem":"repair_required"}"#);
        let uri = super::super::tests_support::sample_managed_uri();
        let _ = authenticate(&uri).await;
        assert!(matches!(
            redeem_and_connect(&uri).await,
            Err(RedeemOutcome::Repair { .. })
        ));
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn stub_redeem_ok_stores_durable_record_without_persisting_credentials() {
        set_config(r#"{"stubAuth":"authenticated","stubRedeem":"ok"}"#);
        let payload = super::super::tests_support::sample_managed_payload();
        let uri = payload.to_uri().expect("encode managed uri");
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::Authenticated { .. }
        ));
        redeem_and_connect(&uri)
            .await
            .expect("stub redeem stores + connects");

        let records = IndexedDbHostStore.list().await.expect("list");
        let record = records
            .iter()
            .find(|record| {
                record
                    .managed
                    .as_ref()
                    .is_some_and(|managed| managed.device_id == "dev_stub")
            })
            .expect("a managed record must be stored");
        // The scanned room + PSK are preserved for the rendezvous.
        assert_eq!(record.room, payload.room, "stored room must match the QR");
        let psk = IndexedDbPskStore
            .load(&record.psk_keychain_key_id)
            .await
            .expect("psk stored");
        assert_eq!(psk, payload.psk, "stored PSK must match the QR");
        // Persisted record must NOT contain any broker credentials (finding #5):
        // the serialized JSON carries only durable identifiers.
        let json = serde_json::to_string(record).expect("serialize record");
        assert!(
            !json.contains("grant_id") && !json.contains("client_id"),
            "no ephemeral broker grant may be persisted: {json}"
        );

        // Clean up so repeated runs stay isolated.
        for record in records {
            if record.managed.is_some() {
                let _ = super::super::forget_paired_host(&record.local_host_id).await;
            }
        }
        set_config("");
    }

    /// The Tyggs sign-in URL must carry the configured provider and a
    /// `return_to` back to the app, so `tycode.dev` can run OAuth server-side and
    /// redirect the handoff marker back here (consensus contract).
    #[wasm_bindgen_test]
    fn sign_in_url_carries_provider_and_return_to() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"apple"}"#);
        let url = tyggs_sign_in_url().expect("a configured build yields a sign-in url");
        assert!(
            url.starts_with("https://tycode.dev/api/tyde/mobile/v1/auth/start?"),
            "sign-in must hit the same-origin /auth/start endpoint: {url}"
        );
        assert!(
            url.contains("provider=apple"),
            "the provider must be in the start url: {url}"
        );
        assert!(
            url.contains("return_to="),
            "return_to must bring the browser back to the app: {url}"
        );
        set_config("");
    }

    /// With no `provider` configured the start URL falls back to the single
    /// default provider rather than omitting the parameter the contract requires.
    #[wasm_bindgen_test]
    fn sign_in_url_defaults_provider() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1"}"#);
        let url = tyggs_sign_in_url().expect("a configured build yields a sign-in url");
        assert!(
            url.contains(&format!("provider={}", DEFAULT_AUTH_PROVIDER.as_str())),
            "the default provider must be applied: {url}"
        );
        set_config("");
    }

    /// The live config/default must use a service-accepted provider rather than
    /// the old product label (`tyggs`), and invalid provider config fails closed.
    #[wasm_bindgen_test]
    fn sign_in_url_rejects_unknown_provider() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"tyggs"}"#);
        assert!(
            tyggs_sign_in_url().is_none(),
            "unsupported provider config must not fall back to a different provider"
        );
        set_config("");
    }

    /// `return_to` must never send the QR fragment to tycode.dev/Tyggs OAuth
    /// plumbing. Only the app URL path/query is preserved, upgraded to HTTPS.
    #[wasm_bindgen_test]
    fn sign_in_url_sanitizes_qr_fragment_from_return_to() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?theme=dark&psk=query-psk#tyde-pair://v2?offer_secret=secret&room=room&psk=fragment-psk"),
            )
            .expect("seed QR fragment");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);

        let sign_in = tyggs_sign_in_url().expect("sign-in url");
        let sign_in = web_sys::Url::new(&sign_in).expect("parse sign-in url");
        let return_to = sign_in
            .search_params()
            .get("return_to")
            .expect("return_to param");

        assert!(
            return_to.starts_with("https://"),
            "return_to must be HTTPS: {return_to}"
        );
        assert!(
            return_to.contains("/tyde/?theme=dark"),
            "safe app path/query must be preserved: {return_to}"
        );
        for secret in ["tyde-pair", "offer_secret", "room=", "psk", "#"] {
            assert!(
                !return_to.contains(secret),
                "return_to leaked {secret:?}: {return_to}"
            );
        }

        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// An unconfigured build must not fabricate a sign-in destination — it fails
    /// closed so the UI never navigates nowhere.
    #[wasm_bindgen_test]
    fn sign_in_url_none_without_base_url() {
        set_config("{}");
        assert!(
            tyggs_sign_in_url().is_none(),
            "no endpoint configured → no sign-in url"
        );
        set_config("");
    }

    /// A `handoffCode` in the query string is read once and stripped from the URL
    /// immediately, so a reload/back-button can't replay the one-time marker.
    #[wasm_bindgen_test]
    fn handoff_code_query_is_read_once_and_stripped() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/?handoffCode=abc123"))
            .expect("seed handoff query");

        assert_eq!(take_handoff_code().as_deref(), Some("abc123"));
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "the handoff marker must be stripped from the URL: {after}"
        );
        assert!(
            take_handoff_code().is_none(),
            "the one-time handoff must not be readable a second time"
        );

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// The fragment form (`#handoffCode=…`) is also read once and stripped — the
    /// fragment keeps the marker off the static origin's request logs.
    #[wasm_bindgen_test]
    fn handoff_code_fragment_is_read_once_and_stripped() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/#handoffCode=frag-xyz"))
            .expect("seed handoff fragment");

        assert_eq!(take_handoff_code().as_deref(), Some("frag-xyz"));
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "the fragment handoff marker must be stripped: {after}"
        );

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// A clean URL yields no handoff — the normal cookie-probe / reconnect path.
    #[wasm_bindgen_test]
    fn handoff_code_absent_is_none() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/"))
            .expect("clear url");

        assert!(take_handoff_code().is_none());

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// OAuth return handling must exchange the one-time handoff marker for the
    /// HttpOnly cookie-only session and strip all QR-secret-bearing URL parts
    /// before the network call completes.
    #[wasm_bindgen_test]
    async fn handoff_success_posts_cookie_only_session_and_strips_qr_url() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?handoffCode=ok#tyde-pair://v2?offer_secret=secret&room=room&psk=psk"),
            )
            .expect("seed handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![(200, r#"{"authenticated":true,"expires_at_ms":42}"#)]);

        let uri = super::super::tests_support::sample_managed_uri();
        match authenticate(&uri).await {
            MobileServiceAuthState::Authenticated { expires_at_ms } => {
                assert_eq!(expires_at_ms, 42);
            }
            other => panic!("expected authenticated handoff, got {other:?}"),
        }
        assert_eq!(
            fetch.calls(),
            vec![FetchCall {
                method: "POST".to_owned(),
                url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
            }],
            "handoff must POST exactly once to /auth/session"
        );
        let after = window.location().href().expect("href");
        for secret in ["handoffCode", "tyde-pair", "offer_secret", "room=", "psk"] {
            assert!(
                !after.contains(secret),
                "handoff cleanup leaked {secret:?}: {after}"
            );
        }

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// A failed one-time handoff surfaces an auth state and still cannot replay:
    /// the marker is consumed before the failed POST returns.
    #[wasm_bindgen_test]
    async fn handoff_failure_consumes_marker_without_storing_token() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=bad"))
            .expect("seed handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![(
            401,
            r#"{"error":{"code":"invalid_tyggs_auth","message":"handoff expired","retryable":false}}"#,
        )]);

        let uri = super::super::tests_support::sample_managed_uri();
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::AuthFailed { .. }
        ));
        assert_eq!(fetch.calls().len(), 1);
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "failed handoff must still strip the marker: {after}"
        );
        assert!(
            take_handoff_code().is_none(),
            "failed handoff must not be replayable"
        );

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// After a successful handoff, a later auth probe must use the cookie-only
    /// `GET /auth/session` path rather than replaying the one-time POST.
    #[wasm_bindgen_test]
    async fn handoff_is_not_replayed_after_success() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=once"))
            .expect("seed handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![
            (200, r#"{"authenticated":true,"expires_at_ms":99}"#),
            (200, r#"{"authenticated":false}"#),
        ]);

        let uri = super::super::tests_support::sample_managed_uri();
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::Authenticated { .. }
        ));
        assert!(matches!(
            authenticate(&uri).await,
            MobileServiceAuthState::AuthFailed { .. }
        ));
        let methods: Vec<String> = fetch.calls().into_iter().map(|call| call.method).collect();
        assert_eq!(
            methods,
            vec!["POST".to_owned(), "GET".to_owned()],
            "handoff POST must not replay after the marker is stripped"
        );

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// Reconnect handoff failure must stop before broker-credential minting and
    /// surface the auth-required state as a typed credential error.
    #[wasm_bindgen_test]
    async fn reconnect_handoff_failure_stops_before_mint() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=expired"))
            .expect("seed reconnect handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![(
            401,
            r#"{"error":{"code":"invalid_tyggs_auth","message":"handoff expired","retryable":false}}"#,
        )]);
        let record = reconnect_record(
            "reconnect-handoff-fail",
            mobile_shell_types::KeychainSecretId("missing-device-secret".to_owned()),
        );

        let error = obtain_managed_credentials(&record, 0)
            .await
            .expect_err("handoff failure must abort reconnect credentials");

        assert_eq!(error.code, MobileAccessErrorCode::ServiceAuthRequired);
        assert!(!error.retryable);
        assert!(
            error.message.contains("Sign in"),
            "auth failure should remain user-actionable: {}",
            error.message
        );
        assert_eq!(
            fetch.calls(),
            vec![FetchCall {
                method: "POST".to_owned(),
                url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
            }],
            "failed handoff must not continue to /broker-credentials"
        );

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// A successful reconnect handoff preserves the intended path: it exchanges
    /// the handoff first, then proceeds to the broker-credentials request.
    #[wasm_bindgen_test]
    async fn reconnect_handoff_success_continues_to_mint() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=fresh"))
            .expect("seed reconnect handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![
            (200, r#"{"authenticated":true,"expires_at_ms":99}"#),
            (
                503,
                r#"{"error":{"code":"service_unavailable","message":"mint outage","retryable":true}}"#,
            ),
        ]);
        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_test")
                .await
                .expect("store device secret");
        let record = reconnect_record("reconnect-handoff-ok", device_secret_key_id.clone());

        let error = obtain_managed_credentials(&record, 0)
            .await
            .expect_err("second mocked service response fails the mint");

        assert_eq!(error.code, MobileAccessErrorCode::ServiceUnavailable);
        let calls = fetch.calls();
        assert_eq!(calls.len(), 2, "handoff success must continue to mint");
        assert_eq!(calls[0].method, "POST");
        assert_eq!(
            calls[0].url,
            "https://tycode.dev/api/tyde/mobile/v1/auth/session"
        );
        assert_eq!(calls[1].method, "POST");
        assert!(
            calls[1]
                .url
                .ends_with("/pairings/pair_01J/broker-credentials"),
            "second call must be broker credential mint: {:?}",
            calls
        );

        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// A successful reconnect mint must preserve the browser-safe AWS IoT
    /// custom-authorizer WebSocket URL in typed credentials. The MQTT layer then
    /// uses this URL instead of falling back to the broker endpoint.
    #[wasm_bindgen_test]
    async fn reconnect_mint_parses_managed_websocket_url() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_test")
                .await
                .expect("store device secret");
        let fetch = install_fetch_mock(vec![(
            200,
            r#"{
                "pairing_id":"pair_01J",
                "status":"active",
                "broker":{
                    "endpoint":"wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
                    "provider":"aws_iot_core",
                    "region":"us-west-2",
                    "authorizer_name":"tycode-mobile-v1"
                },
                "broker_credentials":{
                    "grant_id":"grant_01J",
                    "client_id":"tyde/prod/pair_01J/mobile/dev_01J/grant_01J",
                    "connect":{
                        "username":"x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant",
                        "password":"signed-grant",
                        "websocket_url":"wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant",
                        "headers":{"x-tycode-grant":"signed-grant"}
                    },
                    "scope":{
                        "namespace":"tyde/prod/pair_01J",
                        "role":"mobile",
                        "publish":["tyde/prod/pair_01J/rooms/+/client-to-host"],
                        "subscribe":["tyde/prod/pair_01J/rooms/+/host-to-client"]
                    },
                    "issued_at_ms":1,
                    "expires_at_ms":4102444800000
                }
            }"#,
        )]);
        let record = reconnect_record("reconnect-websocket-url", device_secret_key_id.clone());

        let (_broker, credentials) = obtain_managed_credentials(&record, 0)
            .await
            .expect("mint credentials");

        assert_eq!(
            credentials
                .connect
                .websocket_url
                .as_ref()
                .map(|url| url.as_str()),
            Some(
                "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant"
            )
        );
        assert_eq!(fetch.calls().len(), 1);

        clear_cached_credentials(&record.local_host_id);
        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
    }

    #[wasm_bindgen_test]
    async fn reconnect_mint_rejects_missing_managed_websocket_url() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_test_missing_url")
                .await
                .expect("store device secret");
        let fetch = install_fetch_mock(vec![(
            200,
            r#"{
                "pairing_id":"pair_01J",
                "status":"active",
                "broker":{
                    "endpoint":"wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
                    "provider":"aws_iot_core",
                    "region":"us-west-2",
                    "authorizer_name":"tycode-mobile-v1"
                },
                "broker_credentials":{
                    "grant_id":"grant_01J",
                    "client_id":"tyde/prod/pair_01J/mobile/dev_01J/grant_01J",
                    "connect":{
                        "username":"x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant",
                        "password":"signed-grant",
                        "headers":{"x-tycode-grant":"signed-grant"}
                    },
                    "scope":{
                        "namespace":"tyde/prod/pair_01J",
                        "role":"mobile",
                        "publish":["tyde/prod/pair_01J/rooms/+/client-to-host"],
                        "subscribe":["tyde/prod/pair_01J/rooms/+/host-to-client"]
                    },
                    "issued_at_ms":1,
                    "expires_at_ms":4102444800000
                }
            }"#,
        )]);
        let record = reconnect_record(
            "reconnect-missing-websocket-url",
            device_secret_key_id.clone(),
        );

        let error = obtain_managed_credentials(&record, 0)
            .await
            .expect_err("missing websocket_url must fail closed");

        assert_eq!(error.code, MobileAccessErrorCode::InvalidConfig);
        assert!(!error.retryable);
        assert!(
            error.message.contains("connect.websocket_url"),
            "missing field must be explicit: {}",
            error.message
        );
        assert_eq!(fetch.calls().len(), 1);

        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
    }
}
