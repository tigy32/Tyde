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
//!   providers: ["apple", "google"],                     // ordered sign-in choices
//!   // Legacy deployed configs may still use provider: "google" | "apple".
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
use tokio::sync::oneshot;
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
const AUTH_CALLBACK_PARAM_KEYS: &[&str] = &[
    "oauth",
    "provider",
    "code",
    "error",
    "error_code",
    "error_description",
    "message",
    "auth",
    "auth_error",
    "oauth_error",
];
const RETURN_TO_SECRET_PARAM_KEYS: &[&str] = &[
    HANDOFF_CODE_KEY,
    "offer_secret",
    "offerSecret",
    "room",
    "psk",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthCallback {
    HandoffCode(String),
    Failed { message: String },
}

enum AuthCallbackExchange {
    Unchecked,
    InFlight(Vec<oneshot::Sender<MobileServiceAuthState>>),
    Complete(Option<MobileServiceAuthState>),
}

enum AuthCallbackExchangeAction {
    Complete(AuthCallback),
    Wait(oneshot::Receiver<MobileServiceAuthState>),
    Ready(Option<MobileServiceAuthState>),
}

/// Client clock-skew plus mint/connect latency margin applied against the
/// service-owned `connect_valid_until_ms` boundary, so a grant that looks
/// connectable on the phone's clock is still inside the boundary when the
/// CONNECT reaches AWS. The authorizer's own minimum-lifetime policy lives in
/// tycode-mobile-service and arrives already folded into
/// `connect_valid_until_ms` — it is deliberately not mirrored here.
const CREDENTIAL_CLOCK_SKEW_ALLOWANCE_MS: u64 = 60_000;

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

    static AUTH_CALLBACK_EXCHANGE: RefCell<AuthCallbackExchange> =
        const { RefCell::new(AuthCallbackExchange::Unchecked) };

    /// Fresh mobile broker grants, held **only in memory** (never persisted —
    /// finding #5 / dev-docs/30). Populated by redeem and mint; reused for a
    /// reconnect that happens while still connectable; dropped on forget or
    /// reload.
    static CREDENTIAL_CACHE: RefCell<HashMap<LocalHostId, CachedBrokerGrant>> =
        RefCell::new(HashMap::new());
}

/// A cached broker grant paired with the service-owned connect-validity
/// boundary from the mint/redeem contract. The boundary is not part of the
/// protocol [`ManagedBrokerCredentials`], so it rides alongside them in this
/// in-memory cache only. No `Debug`: the credentials carry secrets.
#[derive(Clone)]
struct CachedBrokerGrant {
    credentials: ManagedBrokerCredentials,
    /// Absolute epoch-ms deadline after which the AWS authorizer will refuse a
    /// CONNECT with this grant, computed service-side (token expiry minus the
    /// authorizer's minimum-lifetime policy).
    connect_valid_until_ms: u64,
}

fn mark_authenticated(value: bool) {
    AUTHENTICATED.with(|flag| flag.set(value));
}

fn is_authenticated() -> bool {
    AUTHENTICATED.with(Cell::get)
}

fn cache_credentials(local_host_id: &LocalHostId, grant: CachedBrokerGrant) {
    CREDENTIAL_CACHE.with(|cache| {
        cache.borrow_mut().insert(local_host_id.clone(), grant);
    });
}

fn cached_connectable_credentials(
    local_host_id: &LocalHostId,
    now_ms: u64,
) -> Option<ManagedBrokerCredentials> {
    CREDENTIAL_CACHE.with(|cache| {
        cache
            .borrow()
            .get(local_host_id)
            .filter(|grant| cached_grant_is_connectable(grant, now_ms))
            .map(|grant| grant.credentials.clone())
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
pub enum AuthProvider {
    Apple,
    Google,
}

impl AuthProvider {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "apple" => Ok(Self::Apple),
            "google" => Ok(Self::Google),
            other => Err(format!("unsupported Tyggs OAuth provider {other:?}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Google => "google",
        }
    }

    pub fn sign_in_label(self) -> &'static str {
        match self {
            Self::Apple => "Continue with Apple",
            Self::Google => "Continue with Google",
        }
    }
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    base_url: Option<String>,
    providers: Result<Vec<AuthProvider>, String>,
    stub_auth: Option<String>,
    stub_redeem: Option<String>,
    paywall_url: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            providers: Ok(vec![DEFAULT_AUTH_PROVIDER]),
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
        providers: read_auth_providers(&config),
        stub_auth: read_string_field(&config, "stubAuth"),
        stub_redeem: read_string_field(&config, "stubRedeem"),
        paywall_url: read_string_field(&config, "paywallUrl"),
    }
}

fn read_auth_providers(config: &JsValue) -> Result<Vec<AuthProvider>, String> {
    if let Ok(value) = js_sys::Reflect::get(config, &JsValue::from_str("providers"))
        && !value.is_undefined()
        && !value.is_null()
    {
        return parse_provider_array(&value);
    }

    if let Ok(value) = js_sys::Reflect::get(config, &JsValue::from_str("provider"))
        && !value.is_undefined()
        && !value.is_null()
    {
        let raw = value.as_string().ok_or_else(|| {
            "managed mobile auth config field `provider` must be a string".to_owned()
        })?;
        let provider = raw.trim();
        if provider.is_empty() {
            return Err("managed mobile auth config field `provider` must not be empty".to_owned());
        }
        return AuthProvider::parse(provider).map(|provider| vec![provider]);
    }

    Ok(vec![DEFAULT_AUTH_PROVIDER])
}

fn parse_provider_array(value: &JsValue) -> Result<Vec<AuthProvider>, String> {
    if !js_sys::Array::is_array(value) {
        return Err(
            "managed mobile auth config field `providers` must be a non-empty array".to_owned(),
        );
    }
    let array = js_sys::Array::from(value);
    if array.length() == 0 {
        return Err("managed mobile auth config field `providers` must not be empty".to_owned());
    }

    let mut providers = Vec::new();
    for index in 0..array.length() {
        let raw = array
            .get(index)
            .as_string()
            .ok_or_else(|| format!("managed mobile auth provider #{index} must be a string"))?;
        let provider = raw.trim();
        if provider.is_empty() {
            return Err(format!(
                "managed mobile auth provider #{index} must not be empty"
            ));
        }
        let provider = AuthProvider::parse(provider)?;
        if providers.contains(&provider) {
            return Err(format!(
                "managed mobile auth provider {provider:?} is configured more than once"
            ));
        }
        providers.push(provider);
    }
    Ok(providers)
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

/// Validates the managed offer, probes the cookie-backed `tycode.dev` session,
/// and returns the typed [`MobileServiceAuthState`]. Boot callback completion is
/// deliberately separate so pairing auth cannot race reconnect credential mint.
pub async fn authenticate(qr_uri: &str) -> MobileServiceAuthState {
    if let Err(message) = parse_managed_offer(qr_uri) {
        return MobileServiceAuthState::AuthFailed { message };
    }
    probe_auth().await
}

/// Completes or joins the page's one OAuth callback exchange. Reconnect minting
/// uses the same single-flight state, so the session cookie always exists before
/// either path continues and the handoff marker has only one consumer.
pub async fn complete_boot_auth_callback() -> Option<MobileServiceAuthState> {
    resolve_auth_callback().await
}

/// Re-probes the current cookie-backed auth state without requiring a QR.
pub async fn probe_auth() -> MobileServiceAuthState {
    let config = load_config();
    let state = match config.base_url.as_deref() {
        Some(base_url) => authenticate_live(base_url, &config).await,
        None => match config.stub_auth.as_deref() {
            Some(kind) => stub_auth_state(kind, &config),
            None => not_configured(),
        },
    };
    AUTH_CALLBACK_EXCHANGE.with(|exchange| {
        let mut exchange = exchange.borrow_mut();
        // Refresh only an ALREADY-COMPLETED exchange so later reconnect mints
        // see the newest probe outcome (e.g. a retried sign-in clearing a stale
        // AuthFailed). An `Unchecked` exchange must stay unchecked: the URL may
        // still carry an unconsumed OAuth callback (`handoffCode`), and caching
        // a probe result over it would mask the source of truth (dev-docs/01).
        // `InFlight` is left for the in-flight exchange to complete.
        if matches!(*exchange, AuthCallbackExchange::Complete(_)) {
            *exchange = AuthCallbackExchange::Complete(Some(state.clone()));
        }
    });
    state
}

async fn resolve_auth_callback() -> Option<MobileServiceAuthState> {
    let action = AUTH_CALLBACK_EXCHANGE.with(|exchange| {
        let mut exchange = exchange.borrow_mut();
        match &mut *exchange {
            AuthCallbackExchange::Unchecked => match take_auth_callback() {
                Some(callback) => {
                    *exchange = AuthCallbackExchange::InFlight(Vec::new());
                    AuthCallbackExchangeAction::Complete(callback)
                }
                None => {
                    *exchange = AuthCallbackExchange::Complete(None);
                    AuthCallbackExchangeAction::Ready(None)
                }
            },
            AuthCallbackExchange::InFlight(waiters) => {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                AuthCallbackExchangeAction::Wait(receiver)
            }
            AuthCallbackExchange::Complete(state) => {
                AuthCallbackExchangeAction::Ready(state.clone())
            }
        }
    });

    match action {
        AuthCallbackExchangeAction::Ready(state) => state,
        AuthCallbackExchangeAction::Wait(receiver) => match receiver.await {
            Ok(state) => Some(state),
            Err(_) => Some(MobileServiceAuthState::ServiceUnavailable {
                message: "Tyggs sign-in callback exchange ended unexpectedly.".to_owned(),
                retryable: true,
            }),
        },
        AuthCallbackExchangeAction::Complete(callback) => {
            let config = load_config();
            let state = complete_auth_callback(&config, callback).await;
            let waiters = AUTH_CALLBACK_EXCHANGE.with(|exchange| {
                match std::mem::replace(
                    &mut *exchange.borrow_mut(),
                    AuthCallbackExchange::Complete(Some(state.clone())),
                ) {
                    AuthCallbackExchange::InFlight(waiters) => waiters,
                    AuthCallbackExchange::Unchecked | AuthCallbackExchange::Complete(_) => {
                        Vec::new()
                    }
                }
            });
            for waiter in waiters {
                let _ = waiter.send(state.clone());
            }
            Some(state)
        }
    }
}

/// Configured OAuth providers for the Tyggs sign-in UI, in display order.
pub fn auth_providers() -> Result<Vec<AuthProvider>, String> {
    load_config().providers
}

/// The full-page URL that starts the Tyggs sign-in through `tycode.dev` for an
/// explicitly selected, configured provider. `return_to` brings the browser back
/// to the current app URL, where the appended one-time `handoffCode` is
/// exchanged for the session cookie (see [`complete_handoff`]).
pub fn tyggs_sign_in_url(provider: AuthProvider) -> Result<String, String> {
    let config = load_config();
    let base_url = config.base_url.clone().ok_or_else(|| {
        "Tyde managed mobile access isn't configured in this build, so sign-in can't start."
            .to_owned()
    })?;
    let providers = config.providers?;
    if !providers.contains(&provider) {
        return Err(format!(
            "Tyggs OAuth provider {:?} is not enabled in this build",
            provider.as_str()
        ));
    }
    let return_to =
        sanitized_return_to_url().ok_or_else(|| "failed to read the current app URL".to_owned())?;
    let encoded_provider = js_sys::encode_uri_component(provider.as_str());
    let encoded_return = js_sys::encode_uri_component(&return_to);
    Ok(format!(
        "{base_url}/auth/start?provider={encoded_provider}&return_to={encoded_return}"
    ))
}

fn sanitized_return_to_url() -> Option<String> {
    let window = web_sys::window()?;
    let href = window.location().href().ok()?;
    let url = web_sys::Url::new(&href).ok()?;
    url.set_protocol("https:");
    strip_return_to_search_params(&url);
    url.set_hash("");
    Some(url.href())
}

fn strip_return_to_search_params(url: &web_sys::Url) {
    strip_search_params(url, RETURN_TO_SECRET_PARAM_KEYS);
    strip_search_params(url, AUTH_CALLBACK_PARAM_KEYS);
}

fn strip_search_params(url: &web_sys::Url, keys: &[&str]) {
    for key in keys {
        url.search_params().delete(key);
    }
}

fn strip_auth_callback_params(url: &web_sys::Url) {
    strip_search_params(url, AUTH_CALLBACK_PARAM_KEYS);
    strip_fragment_params(url, AUTH_CALLBACK_PARAM_KEYS);
}

fn strip_fragment_params(url: &web_sys::Url, keys: &[&str]) {
    let hash = url.hash();
    let fragment = hash.strip_prefix('#').unwrap_or(&hash);
    if fragment.is_empty() {
        return;
    }

    let mut kept = Vec::new();
    let mut changed = false;
    for part in fragment.split('&') {
        let key = part.split_once('=').map_or(part, |(key, _)| key);
        let key = decode_uri_component(key);
        if keys.contains(&key.as_str()) {
            changed = true;
        } else {
            kept.push(part);
        }
    }
    if !changed {
        return;
    }
    if kept.is_empty() {
        url.set_hash("");
    } else {
        url.set_hash(&kept.join("&"));
    }
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
/// cached grant while its service-owned `connect_valid_until_ms` boundary
/// (minus a clock-skew allowance) has not passed, otherwise mints new ones via
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
    if let Some(credentials) = cached_connectable_credentials(&record.local_host_id, now_ms) {
        return Ok((managed.broker.clone(), credentials));
    }
    mint_managed_credentials(record, managed).await
}

// ── Live HTTP: auth ──────────────────────────────────────────────────────────

async fn authenticate_live(base_url: &str, config: &ServiceConfig) -> MobileServiceAuthState {
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

async fn complete_auth_callback(
    config: &ServiceConfig,
    callback: AuthCallback,
) -> MobileServiceAuthState {
    match callback {
        AuthCallback::HandoffCode(handoff_code) => match config.base_url.as_deref() {
            Some(base_url) => complete_handoff(base_url, config, &handoff_code).await,
            None => not_configured(),
        },
        AuthCallback::Failed { message } => {
            mark_authenticated(false);
            MobileServiceAuthState::AuthFailed { message }
        }
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
            handoff_auth_state_from_error(&body, config)
        }
        Err(message) => MobileServiceAuthState::ServiceUnavailable {
            message,
            retryable: true,
        },
    }
}

/// Captures a Tyggs OAuth callback from the current URL and removes its markers
/// via `history.replaceState` before any network call. Both successful handoff
/// markers and OAuth failures are one-shot: neither can replay on reload/back.
fn take_auth_callback() -> Option<AuthCallback> {
    let window = web_sys::window()?;
    let location = window.location();
    let href = location.href().ok()?;
    let url = web_sys::Url::new(&href).ok()?;

    let callback = capture_auth_callback(&url)?;
    strip_return_to_search_params(&url);
    match callback {
        AuthCallback::HandoffCode(_) => url.set_hash(""),
        AuthCallback::Failed { .. } => {
            strip_auth_callback_params(&url);
            strip_fragment_params(&url, RETURN_TO_SECRET_PARAM_KEYS);
            if url.hash().contains("tyde-pair://") {
                url.set_hash("");
            }
        }
    }
    replace_url(&window, &url);
    Some(callback)
}

fn capture_auth_callback(url: &web_sys::Url) -> Option<AuthCallback> {
    // Presence means a NON-EMPTY value: a bare `?code=` or `#oauth` must never
    // read as an OAuth callback.
    let param = |key: &str| auth_callback_param(url, key).filter(|value| !value.is_empty());
    // The Tyggs OAuth redirect always carries an explicit `oauth=…` marker
    // (`?oauth=success&provider=…`). Generic keys like `error` and `code` occur
    // in plenty of non-OAuth URLs, so without that marker they must not hijack
    // a normal boot into a Tyggs failure. Explicitly auth-named keys
    // (`auth_error`, `oauth_error`) are unambiguous and stand on their own.
    let oauth_marker = param("oauth").is_some();

    let error_code = ["auth_error", "oauth_error"]
        .into_iter()
        .find_map(&param)
        .or_else(|| {
            if !oauth_marker {
                return None;
            }
            ["error_code", "error"].into_iter().find_map(&param)
        });
    if let Some(error_code) = error_code {
        let detail = ["error_description", "message"]
            .into_iter()
            .find_map(&param);
        let message = match detail {
            Some(detail) if detail != error_code => {
                format!("Tyggs sign-in failed ({error_code}): {detail}")
            }
            _ => format!("Tyggs sign-in failed: {error_code}"),
        };
        return Some(AuthCallback::Failed { message });
    }

    if let Some(handoff_code) = param(HANDOFF_CODE_KEY) {
        return Some(AuthCallback::HandoffCode(handoff_code));
    }

    if oauth_marker {
        return Some(AuthCallback::Failed {
            message: "Tyggs sign-in failed: the OAuth callback did not include a handoff code."
                .to_owned(),
        });
    }

    None
}

fn auth_callback_param(url: &web_sys::Url, name: &str) -> Option<String> {
    let search_value = url.search_params().get(name);
    if search_value.as_ref().is_some_and(|value| !value.is_empty()) {
        return search_value;
    }
    let hash = url.hash();
    let fragment = hash.strip_prefix('#').unwrap_or(&hash);
    let fragment_value = fragment.split('&').find_map(|part| {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        (decode_uri_component(key) == name).then(|| decode_uri_component(value))
    });
    fragment_value.or(search_value)
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
    if let Some(grant) = result.mobile_broker_credentials {
        cache_credentials(&local_host_id, grant);
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
    // Join any boot callback exchange before minting. Terminal auth/pass states
    // stop reconnect, while a retryable service failure falls through so this
    // real mint request can recover after a transient outage without a reload.
    if let Some(state) = resolve_auth_callback().await {
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
            let connect_valid_until_ms = response.broker_credentials.connect_valid_until_ms;
            let credentials = response
                .broker_credentials
                .into_protocol(&broker)
                .map_err(mint_conv_err)?;
            // Cache in memory for the next reconnect within its service-owned
            // connect boundary; never persisted (finding #5).
            cache_credentials(
                &record.local_host_id,
                CachedBrokerGrant {
                    credentials: credentials.clone(),
                    connect_valid_until_ms,
                },
            );
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

fn cached_grant_is_connectable(grant: &CachedBrokerGrant, now_ms: u64) -> bool {
    grant.connect_valid_until_ms > now_ms.saturating_add(CREDENTIAL_CLOCK_SKEW_ALLOWANCE_MS)
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
        MobileServiceAuthState::ServiceUnavailable {
            retryable: true, ..
        } => Ok(()),
        MobileServiceAuthState::ServiceUnavailable {
            message,
            retryable: false,
        } => Err(ManagedCredentialError {
            code: MobileAccessErrorCode::ServiceUnavailable,
            message,
            retryable: false,
        }),
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
    auth_state_from_error_body(error, config)
}

fn handoff_auth_state_from_error(body: &str, config: &ServiceConfig) -> MobileServiceAuthState {
    let Some(error) = parse_error(body) else {
        return MobileServiceAuthState::ServiceUnavailable {
            message: "tycode.dev returned an unexpected error.".to_owned(),
            retryable: true,
        };
    };
    match error.code.as_str() {
        "invalid_tyggs_auth" | "mobile_session_required" => MobileServiceAuthState::AuthFailed {
            message: error.message,
        },
        _ => auth_state_from_error_body(error, config),
    }
}

fn auth_state_from_error_body(error: ErrorBody, config: &ServiceConfig) -> MobileServiceAuthState {
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
    mobile_broker_credentials: Option<CachedBrokerGrant>,
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
        let connect_valid_until_ms = self.mobile_broker_credentials.connect_valid_until_ms;
        let credentials = self.mobile_broker_credentials.into_protocol(&broker)?;
        Ok(RedeemResult {
            pairing_id: self.pairing_id,
            device_id: self.device_id,
            device_pairing_secret: self.device_pairing_secret,
            broker,
            mobile_broker_credentials: Some(CachedBrokerGrant {
                credentials,
                connect_valid_until_ms,
            }),
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
    /// Service-owned absolute connect-validity boundary: token expiry minus
    /// the authorizer's minimum-lifetime policy, computed by
    /// tycode-mobile-service. Required — a response without it is unreadable.
    connect_valid_until_ms: u64,
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

    /// Freshness is the service-owned `connect_valid_until_ms` boundary minus
    /// only a client clock-skew allowance — no authorizer policy is mirrored
    /// client-side. A grant whose token has not expired but whose connect
    /// boundary is inside the skew window must not be reused.
    #[test]
    fn cached_grant_connectable_respects_service_boundary_and_skew() {
        let now = 1_000_000;
        let grant = |connect_valid_until_ms| CachedBrokerGrant {
            // Token expiry far in the future: connectability must come from
            // the service boundary, never from `expires_at_ms`.
            credentials: sample_credentials(now + 900_000),
            connect_valid_until_ms,
        };
        for boundary_ms in [0, now, now + CREDENTIAL_CLOCK_SKEW_ALLOWANCE_MS] {
            assert!(
                !cached_grant_is_connectable(&grant(boundary_ms), now),
                "boundary at {boundary_ms}ms is at or inside the skew window and must be stale"
            );
        }
        assert!(
            cached_grant_is_connectable(&grant(now + CREDENTIAL_CLOCK_SKEW_ALLOWANCE_MS + 1), now),
            "a boundary strictly beyond the skew window is connectable"
        );
    }

    /// The mint/redeem contract requires `connect_valid_until_ms`; a response
    /// without it must fail to parse (surfaced as an unreadable response),
    /// never default to a client-guessed boundary.
    #[test]
    fn contract_credentials_require_connect_valid_until_ms() {
        let with_field = serde_json::json!({
            "grant_id": "grant_1",
            "client_id": "tyde/prod/pair_1/mobile/dev_1/grant_1",
            "connect": {},
            "scope": {
                "namespace": "tyde/prod/pair_1",
                "role": "mobile",
                "publish": [],
                "subscribe": []
            },
            "issued_at_ms": 0,
            "connect_valid_until_ms": 600_000,
            "expires_at_ms": 900_000
        });
        let parsed: ContractCredentials =
            serde_json::from_value(with_field.clone()).expect("full contract parses");
        assert_eq!(parsed.connect_valid_until_ms, 600_000);

        let mut without_field = with_field;
        without_field
            .as_object_mut()
            .expect("object")
            .remove("connect_valid_until_ms");
        assert!(
            serde_json::from_value::<ContractCredentials>(without_field).is_err(),
            "a credential response without connect_valid_until_ms must be unreadable"
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
            connect_valid_until_ms: 0,
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
        AUTH_CALLBACK_EXCHANGE.with(|exchange| {
            *exchange.borrow_mut() = AuthCallbackExchange::Unchecked;
        });
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
        delayed_resolve: Option<Rc<RefCell<Option<js_sys::Function>>>>,
        delayed_response: Option<(u16, &'static str)>,
        _closure: Closure<dyn FnMut(JsValue) -> js_sys::Promise>,
    }

    impl FetchMock {
        fn calls(&self) -> Vec<FetchCall> {
            self.calls.borrow().clone()
        }

        fn release_delayed_response(&self) {
            let resolve = self
                .delayed_resolve
                .as_ref()
                .expect("delayed fetch mock")
                .borrow_mut()
                .take()
                .expect("delayed request must be pending");
            let (status, body) = self.delayed_response.expect("delayed response");
            resolve
                .call1(&JsValue::NULL, &response_value(status, body))
                .expect("resolve delayed response");
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
            delayed_resolve: None,
            delayed_response: None,
            _closure: closure,
        }
    }

    fn install_delayed_first_fetch_mock(
        first_response: (u16, &'static str),
        remaining_responses: Vec<(u16, &'static str)>,
    ) -> FetchMock {
        let window = web_sys::window().expect("window");
        let original_fetch = js_sys::Reflect::get(&window, &JsValue::from_str("fetch"))
            .expect("read original fetch");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_for_fetch = calls.clone();
        let delayed_resolve = Rc::new(RefCell::new(None));
        let delayed_resolve_for_fetch = delayed_resolve.clone();
        let response_queue = Rc::new(RefCell::new(VecDeque::from(remaining_responses)));
        let responses_for_fetch = response_queue.clone();
        let mut first = true;
        let closure = Closure::<dyn FnMut(JsValue) -> js_sys::Promise>::new(
            move |request_value: JsValue| {
                let request: web_sys::Request = request_value
                    .dyn_into()
                    .expect("managed-service client must call fetch with Request");
                calls_for_fetch.borrow_mut().push(FetchCall {
                    method: request.method(),
                    url: request.url(),
                });
                if first {
                    first = false;
                    let delayed_resolve = delayed_resolve_for_fetch.clone();
                    return js_sys::Promise::new(&mut |resolve, _reject| {
                        *delayed_resolve.borrow_mut() = Some(resolve);
                    });
                }
                let (status, body) = responses_for_fetch.borrow_mut().pop_front().unwrap_or((
                    500,
                    r#"{"error":{"code":"internal","message":"unexpected extra request","retryable":true}}"#,
                ));
                response_promise(status, body)
            },
        );
        js_sys::Reflect::set(&window, &JsValue::from_str("fetch"), closure.as_ref())
            .expect("install fetch mock");
        FetchMock {
            calls,
            original_fetch,
            delayed_resolve: Some(delayed_resolve),
            delayed_response: Some(first_response),
            _closure: closure,
        }
    }

    fn response_promise(status: u16, body: &str) -> js_sys::Promise {
        js_sys::Promise::resolve(&response_value(status, body))
    }

    fn response_value(status: u16, body: &str) -> JsValue {
        js_sys::Function::new_with_args("body,status", "return new Response(body, { status });")
            .call2(
                &JsValue::NULL,
                &JsValue::from_str(body),
                &JsValue::from_f64(f64::from(status)),
            )
            .expect("create Response")
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .expect("window")
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .expect("schedule tick");
        });
        JsFuture::from(promise).await.expect("event-loop tick");
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

    const VALID_MINT_RESPONSE: &str = r#"{
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
            "connect_valid_until_ms":4102444500000,
            "expires_at_ms":4102444800000
        }
    }"#;

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

    /// The Tyggs sign-in URL must carry the explicitly selected provider and a
    /// `return_to` back to the app, so `tycode.dev` can run OAuth server-side
    /// and redirect the handoff marker back here (consensus contract).
    #[wasm_bindgen_test]
    fn sign_in_url_carries_explicit_provider_and_return_to() {
        set_config(
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","google"]}"#,
        );
        let url = tyggs_sign_in_url(AuthProvider::Apple)
            .expect("a configured build yields a sign-in url");
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

    #[wasm_bindgen_test]
    fn provider_list_parses_ordered_apple_then_google() {
        set_config(
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","google"]}"#,
        );

        assert_eq!(
            auth_providers().expect("valid provider list"),
            vec![AuthProvider::Apple, AuthProvider::Google]
        );
        let apple = tyggs_sign_in_url(AuthProvider::Apple).expect("apple url");
        let google = tyggs_sign_in_url(AuthProvider::Google).expect("google url");
        assert!(
            apple.contains("provider=apple"),
            "Apple sign-in must select Apple: {apple}"
        );
        assert!(
            google.contains("provider=google"),
            "Google sign-in must select Google: {google}"
        );

        set_config("");
    }

    /// Legacy single-provider config remains supported for already-deployed
    /// loader shells.
    #[wasm_bindgen_test]
    fn legacy_provider_config_produces_single_choice() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);

        assert_eq!(
            auth_providers().expect("legacy provider config"),
            vec![AuthProvider::Google]
        );
        let url = tyggs_sign_in_url(AuthProvider::Google).expect("google url");
        assert!(
            url.contains("provider=google"),
            "legacy config must still build a Google URL: {url}"
        );
        assert!(
            tyggs_sign_in_url(AuthProvider::Apple).is_err(),
            "legacy Google-only config must not allow Apple implicitly"
        );

        set_config("");
    }

    /// With no provider config the start URL falls back to the single default
    /// provider rather than omitting the parameter the contract requires.
    #[wasm_bindgen_test]
    fn sign_in_url_defaults_provider() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1"}"#);
        assert_eq!(
            auth_providers().expect("default provider"),
            vec![DEFAULT_AUTH_PROVIDER]
        );
        let url = tyggs_sign_in_url(DEFAULT_AUTH_PROVIDER)
            .expect("a configured build yields a sign-in url");
        assert!(
            url.contains(&format!("provider={}", DEFAULT_AUTH_PROVIDER.as_str())),
            "the default provider must be applied: {url}"
        );
        set_config("");
    }

    /// The live config/default must use a service-accepted provider rather than
    /// the old product label (`tyggs`), and invalid provider config fails closed.
    #[wasm_bindgen_test]
    fn provider_config_errors_fail_closed() {
        for config in [
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"tyggs"}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":""}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":7}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":[]}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","apple"]}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","tyggs"],"provider":"google"}"#,
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":"google","provider":"google"}"#,
        ] {
            set_config(config);
            assert!(
                auth_providers().is_err(),
                "invalid provider config must surface an error: {config}"
            );
            assert!(
                tyggs_sign_in_url(AuthProvider::Google).is_err(),
                "invalid provider config must not fall back to Google: {config}"
            );
            assert!(
                tyggs_sign_in_url(AuthProvider::Apple).is_err(),
                "invalid provider config must not fall back to Apple: {config}"
            );
            set_config("");
        }
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
                Some("/tyde/?theme=dark&handoffCode=stale&offer_secret=query-secret&offerSecret=query-secret&room=query-room&psk=query-psk#tyde-pair://v2?offer_secret=secret&room=room&psk=fragment-psk"),
            )
            .expect("seed QR fragment");
        set_config(
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","google"]}"#,
        );

        let sign_in = tyggs_sign_in_url(AuthProvider::Google).expect("sign-in url");
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
        let return_url = web_sys::Url::new(&return_to).expect("parse return_to");
        for key in RETURN_TO_SECRET_PARAM_KEYS {
            assert!(
                return_url.search_params().get(*key).is_none(),
                "return_to leaked query secret {key:?}: {return_to}"
            );
        }

        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// Stale Tyggs/Account OAuth callback params in the current app URL must
    /// not be embedded in a new `return_to`, or Account rejects the handoff.
    #[wasm_bindgen_test]
    fn sign_in_url_cleans_oauth_callback_params_from_return_to() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?theme=dark&oauth=1&provider=google&code=abc&error=bad&error_code=e&error_description=desc&message=msg&auth=1&auth_error=auth&oauth_error=oauth"),
            )
            .expect("seed OAuth callback params");
        set_config(
            r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","providers":["apple","google"]}"#,
        );

        let sign_in = tyggs_sign_in_url(AuthProvider::Google).expect("sign-in url");
        let sign_in = web_sys::Url::new(&sign_in).expect("parse sign-in url");
        let return_to = sign_in
            .search_params()
            .get("return_to")
            .expect("return_to param");
        let return_url = web_sys::Url::new(&return_to).expect("parse return_to");

        assert_eq!(
            return_url.search_params().get("theme").as_deref(),
            Some("dark"),
            "safe app params must be preserved: {return_to}"
        );
        for key in AUTH_CALLBACK_PARAM_KEYS {
            assert!(
                return_url.search_params().get(*key).is_none(),
                "return_to leaked stale OAuth callback param {key:?}: {return_to}"
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
            tyggs_sign_in_url(AuthProvider::Google).is_err(),
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

        assert_eq!(
            take_auth_callback(),
            Some(AuthCallback::HandoffCode("abc123".to_owned()))
        );
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "the handoff marker must be stripped from the URL: {after}"
        );
        assert!(
            take_auth_callback().is_none(),
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

        assert_eq!(
            take_auth_callback(),
            Some(AuthCallback::HandoffCode("frag-xyz".to_owned()))
        );
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

        assert!(take_auth_callback().is_none());

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// Generic `code`/`error` params (and bare/empty markers) without the Tyggs
    /// OAuth `oauth=…` marker must not hijack a normal boot into a Tyggs
    /// failure — and must be left in the URL for whatever owns them.
    #[wasm_bindgen_test]
    fn non_oauth_urls_are_not_captured_as_auth_callbacks() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");

        for (seed, survivor) in [
            ("/tyde/?code=abc", Some("code=abc")),
            ("/tyde/?error=1", Some("error=1")),
            ("/tyde/?error_code=boom", Some("error_code=boom")),
            ("/tyde/?code=", None),
            ("/tyde/?oauth=", None),
            ("/tyde/#oauth", None),
            ("/tyde/?auth=1", Some("auth=1")),
        ] {
            history
                .replace_state_with_url(&JsValue::NULL, "", Some(seed))
                .expect("seed non-oauth url");
            assert!(
                take_auth_callback().is_none(),
                "{seed} must not read as an OAuth callback"
            );
            if let Some(survivor) = survivor {
                let after = window.location().href().expect("href");
                assert!(
                    after.contains(survivor),
                    "non-OAuth param {survivor:?} must survive untouched: {after}"
                );
            }
        }

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// With the `oauth=…` marker present the intended callback handling is
    /// preserved: generic `error` maps to a failure, a marked callback missing
    /// its handoff code fails explicitly, and explicitly auth-named error keys
    /// still stand on their own.
    #[wasm_bindgen_test]
    fn oauth_marker_preserves_intended_callback_handling() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");

        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?oauth=success&provider=google&error=denied"),
            )
            .expect("seed marked error callback");
        match take_auth_callback() {
            Some(AuthCallback::Failed { message }) => {
                assert!(message.contains("denied"), "{message}")
            }
            other => panic!("expected marked OAuth error to be captured, got {other:?}"),
        }

        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?oauth=success&provider=google&code=oauth_handoff"),
            )
            .expect("seed marked callback without handoff");
        match take_auth_callback() {
            Some(AuthCallback::Failed { message }) => {
                assert!(
                    message.contains("did not include a handoff code"),
                    "{message}"
                )
            }
            other => panic!("expected missing-handoff failure, got {other:?}"),
        }

        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?auth_error=denied"))
            .expect("seed explicit auth_error");
        match take_auth_callback() {
            Some(AuthCallback::Failed { message }) => {
                assert!(message.contains("denied"), "{message}")
            }
            other => panic!("expected standalone auth_error capture, got {other:?}"),
        }

        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// A cookie probe that runs before the boot callback exchange must not cache
    /// its outcome over the `Unchecked` exchange — the URL may still carry the
    /// unconsumed one-time `handoffCode`, which stays authoritative.
    #[wasm_bindgen_test]
    async fn probe_auth_does_not_mask_unconsumed_url_callback() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=late"))
            .expect("seed unconsumed handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![
            (200, r#"{"authenticated":false}"#),
            (200, r#"{"authenticated":true,"expires_at_ms":7}"#),
        ]);

        // Probe first (signed-out cookie): must NOT complete the exchange.
        assert!(matches!(
            probe_auth().await,
            MobileServiceAuthState::AuthFailed { .. }
        ));

        // The boot callback still finds and exchanges the URL handoff.
        match complete_boot_auth_callback().await {
            Some(MobileServiceAuthState::Authenticated { expires_at_ms }) => {
                assert_eq!(expires_at_ms, 7);
            }
            other => panic!("expected the URL handoff to win over the cached probe, got {other:?}"),
        }
        let methods: Vec<String> = fetch.calls().into_iter().map(|call| call.method).collect();
        assert_eq!(
            methods,
            vec!["GET".to_owned(), "POST".to_owned()],
            "probe must GET, then the handoff must still POST /auth/session"
        );
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "the consumed handoff must be stripped: {after}"
        );

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// An OAuth error callback is captured into an explicit failure before its
    /// params are stripped. It must not degrade into a signed-out cookie probe.
    #[wasm_bindgen_test]
    async fn oauth_error_callback_is_surfaced_then_stripped() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?theme=dark&oauth=1&provider=google&error=oauth_no_linked_account&error_description=No%20linked%20account#oauth=frag&keep=1"),
            )
            .expect("seed OAuth error callback");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(Vec::new());

        match complete_boot_auth_callback().await {
            Some(MobileServiceAuthState::AuthFailed { message }) => {
                assert!(message.contains("oauth_no_linked_account"), "{message}");
                assert!(message.contains("No linked account"), "{message}");
            }
            other => panic!("expected explicit OAuth failure, got {other:?}"),
        }
        assert!(
            fetch.calls().is_empty(),
            "OAuth errors must not probe signed-out state"
        );

        let after = window.location().href().expect("href");
        let after_url = web_sys::Url::new(&after).expect("parse cleaned url");
        assert_eq!(
            after_url.search_params().get("theme").as_deref(),
            Some("dark"),
            "safe app query state must survive cleanup: {after}"
        );
        assert_eq!(
            after_url.hash(),
            "#keep=1",
            "non-callback fragment state must survive cleanup: {after}"
        );
        for key in AUTH_CALLBACK_PARAM_KEYS {
            assert!(
                after_url.search_params().get(*key).is_none(),
                "visible URL leaked query callback param {key:?}: {after}"
            );
            assert!(
                !after_url.hash().contains(*key),
                "visible URL leaked fragment callback param {key:?}: {after}"
            );
        }
        assert!(
            matches!(
                complete_boot_auth_callback().await,
                Some(MobileServiceAuthState::AuthFailed { .. })
            ),
            "a captured OAuth error callback must not replay"
        );
        assert!(
            fetch.calls().is_empty(),
            "replay check must not issue a request"
        );

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// OAuth return handling must exchange the one-time handoff marker for the
    /// HttpOnly cookie-only session and strip all QR-secret-bearing URL parts
    /// before the network call completes.
    #[wasm_bindgen_test]
    async fn boot_handoff_without_pending_qr_posts_session_once() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(
                &JsValue::NULL,
                "",
                Some("/tyde/?handoffCode=ok&oauth=1&provider=google&code=abc&offer_secret=query-secret&room=query-room&psk=query-psk#tyde-pair://v2?offer_secret=secret&room=room&psk=psk"),
            )
            .expect("seed handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![(200, r#"{"authenticated":true,"expires_at_ms":42}"#)]);

        match complete_boot_auth_callback().await {
            Some(MobileServiceAuthState::Authenticated { expires_at_ms }) => {
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
        let after_url = web_sys::Url::new(&after).expect("parse cleaned url");
        for key in AUTH_CALLBACK_PARAM_KEYS {
            assert!(
                after_url.search_params().get(*key).is_none(),
                "handoff cleanup leaked callback param {key:?}: {after}"
            );
        }
        assert!(
            matches!(
                complete_boot_auth_callback().await,
                Some(MobileServiceAuthState::Authenticated { .. })
            ),
            "a successful boot handoff must not replay after URL cleanup"
        );
        assert_eq!(fetch.calls().len(), 1, "handoff POST must remain one-shot");

        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    /// When the pending QR survives the OAuth redirect, the callback exchange
    /// still runs once and the existing managed path redeems the offer once.
    #[wasm_bindgen_test]
    async fn handoff_with_pending_qr_exchanges_once_then_redeems_once() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=pending"))
            .expect("seed pending-QR handoff");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![
            (200, r#"{"authenticated":true,"expires_at_ms":42}"#),
            (
                402,
                r#"{"error":{"code":"pass_required","message":"Pass required after redeem","retryable":false,"paywall_url":"https://tyggs.com/pass"}}"#,
            ),
        ]);

        let uri = super::super::tests_support::sample_managed_uri();
        assert!(matches!(
            complete_boot_auth_callback().await,
            Some(MobileServiceAuthState::Authenticated { .. })
        ));
        assert!(matches!(
            redeem_and_connect(&uri).await,
            Err(RedeemOutcome::Auth(
                MobileServiceAuthState::PassRequired { .. }
            ))
        ));
        assert_eq!(
            fetch.calls(),
            vec![
                FetchCall {
                    method: "POST".to_owned(),
                    url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
                },
                FetchCall {
                    method: "POST".to_owned(),
                    url: "https://tycode.dev/api/tyde/mobile/v1/pairings/redeem".to_owned(),
                },
            ],
            "pending-QR resume must exchange once, then redeem once"
        );

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

        match complete_boot_auth_callback().await {
            Some(MobileServiceAuthState::AuthFailed { message }) => {
                assert_eq!(message, "handoff expired");
            }
            other => panic!("expected explicit handoff failure, got {other:?}"),
        }
        assert_eq!(fetch.calls().len(), 1);
        let after = window.location().href().expect("href");
        assert!(
            !after.contains("handoffCode"),
            "failed handoff must still strip the marker: {after}"
        );
        assert!(
            take_auth_callback().is_none(),
            "failed handoff must not be replayable"
        );
        assert!(
            matches!(
                complete_boot_auth_callback().await,
                Some(MobileServiceAuthState::AuthFailed { .. })
            ),
            "failed boot handoff must not replay through authenticate"
        );
        assert_eq!(fetch.calls().len(), 1, "failed handoff POST must run once");

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
            complete_boot_auth_callback().await,
            Some(MobileServiceAuthState::Authenticated { .. })
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
        assert_eq!(
            error.message, "handoff expired",
            "callback failure must preserve the explicit service error"
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

    /// Boot callback completion and auto-connect share one in-flight exchange.
    /// Boot starts first and the auth response is held pending; auto-connect must
    /// join it without issuing a credential mint until that response is released.
    #[wasm_bindgen_test]
    async fn boot_callback_and_auto_connect_share_one_exchange_before_mint() {
        let window = web_sys::window().expect("window");
        let history = window.history().expect("history");
        let original = window.location().href().expect("href");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some("/tyde/?handoffCode=shared"))
            .expect("seed shared callback");
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_delayed_first_fetch_mock(
            (200, r#"{"authenticated":true,"expires_at_ms":99}"#),
            vec![(
                503,
                r#"{"error":{"code":"service_unavailable","message":"mint outage","retryable":true}}"#,
            )],
        );
        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_concurrent")
                .await
                .expect("store device secret");
        let record = reconnect_record("reconnect-concurrent-handoff", device_secret_key_id.clone());

        let boot_result = Rc::new(RefCell::new(None));
        let boot_result_for_task = boot_result.clone();
        wasm_bindgen_futures::spawn_local(async move {
            *boot_result_for_task.borrow_mut() = Some(complete_boot_auth_callback().await);
        });
        next_tick().await;
        assert_eq!(
            fetch.calls(),
            vec![FetchCall {
                method: "POST".to_owned(),
                url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
            }],
            "boot must start exactly one delayed session exchange"
        );

        let credential_result = Rc::new(RefCell::new(None));
        let credential_result_for_task = credential_result.clone();
        let record_for_task = record.clone();
        wasm_bindgen_futures::spawn_local(async move {
            *credential_result_for_task.borrow_mut() =
                Some(obtain_managed_credentials(&record_for_task, 0).await);
        });
        next_tick().await;
        assert_eq!(
            fetch.calls().len(),
            1,
            "auto-connect must not mint while the session exchange is pending"
        );
        assert!(credential_result.borrow().is_none());

        fetch.release_delayed_response();
        for _ in 0..5 {
            next_tick().await;
        }
        let boot_auth = boot_result
            .borrow_mut()
            .take()
            .expect("boot callback task completed");
        let credential_result = credential_result
            .borrow_mut()
            .take()
            .expect("credential task completed");
        assert!(matches!(
            boot_auth,
            Some(MobileServiceAuthState::Authenticated { .. })
        ));
        assert_eq!(
            credential_result.expect_err("mocked mint must fail").code,
            MobileAccessErrorCode::ServiceUnavailable
        );
        assert_eq!(
            fetch.calls(),
            vec![
                FetchCall {
                    method: "POST".to_owned(),
                    url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
                },
                FetchCall {
                    method: "POST".to_owned(),
                    url:
                        "https://tycode.dev/api/tyde/mobile/v1/pairings/pair_01J/broker-credentials"
                            .to_owned(),
                },
            ],
            "one auth exchange must finish before the single credential mint"
        );

        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
        history
            .replace_state_with_url(&JsValue::NULL, "", Some(&original))
            .expect("restore url");
    }

    #[wasm_bindgen_test]
    async fn transient_auth_probe_does_not_veto_recovered_credential_mint() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![
            (
                503,
                r#"{"error":{"code":"service_unavailable","message":"auth outage","retryable":true}}"#,
            ),
            (200, VALID_MINT_RESPONSE),
        ]);

        assert!(matches!(
            probe_auth().await,
            MobileServiceAuthState::ServiceUnavailable {
                retryable: true,
                ..
            }
        ));

        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_after_auth_outage")
                .await
                .expect("store device secret");
        let record = reconnect_record("reconnect-after-auth-outage", device_secret_key_id.clone());
        obtain_managed_credentials(&record, 0)
            .await
            .expect("recovered service must receive a real credential mint");

        assert_eq!(
            fetch.calls(),
            vec![
                FetchCall {
                    method: "GET".to_owned(),
                    url: "https://tycode.dev/api/tyde/mobile/v1/auth/session".to_owned(),
                },
                FetchCall {
                    method: "POST".to_owned(),
                    url:
                        "https://tycode.dev/api/tyde/mobile/v1/pairings/pair_01J/broker-credentials"
                            .to_owned(),
                },
            ],
            "a cached transient probe failure must not suppress the recovered mint"
        );

        clear_cached_credentials(&record.local_host_id);
        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
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
                    "connect_valid_until_ms":4102444500000,
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

    fn cached_test_grant(
        grant_id: &str,
        connect_valid_until_ms: u64,
        expires_at_ms: u64,
    ) -> CachedBrokerGrant {
        CachedBrokerGrant {
            credentials: cached_test_credentials(grant_id, expires_at_ms),
            connect_valid_until_ms,
        }
    }

    fn cached_test_credentials(grant_id: &str, expires_at_ms: u64) -> ManagedBrokerCredentials {
        ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new(grant_id).expect("grant id"),
            client_id: ManagedBrokerClientId::new("tyde/prod/pair_01J/mobile/dev_01J/grant_cached")
                .expect("client id"),
            connect: ManagedBrokerConnectAuth {
                username: None,
                password: None,
                websocket_url: Some(
                    protocol::BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=cached-grant",
                    )
                    .expect("websocket url"),
                ),
                headers: Default::default(),
            },
            scope: ManagedBrokerCredentialScope {
                namespace: ManagedBrokerTopicNamespace::new("tyde/prod/pair_01J")
                    .expect("namespace"),
                role: ManagedBrokerRole::Mobile,
                publish: vec!["tyde/prod/pair_01J/rooms/+/client-to-host".to_owned()],
                subscribe: vec!["tyde/prod/pair_01J/rooms/+/host-to-client".to_owned()],
            },
            issued_at_ms: 0,
            expires_at_ms,
        }
    }

    /// The latent credential bug: a cached grant whose token has not expired
    /// but whose service-owned connect boundary has passed must be re-minted,
    /// never reused — the authorizer would refuse the connect.
    #[wasm_bindgen_test]
    async fn cached_grant_past_service_connect_boundary_is_reminted_not_reused() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(vec![(200, VALID_MINT_RESPONSE)]);
        let device_secret_key_id =
            super::super::store::store_device_secret("device_pairing_secret_stale_grant")
                .await
                .expect("store device secret");
        let record = reconnect_record("reconnect-stale-grant", device_secret_key_id.clone());
        let now = 1_000_000_000_u64;
        // Token still valid for 299s, but the service says CONNECT stopped
        // being acceptable 1s ago — connectability must follow the boundary,
        // not the token expiry.
        cache_credentials(
            &record.local_host_id,
            cached_test_grant("grant_stale", now - 1_000, now + 299_000),
        );

        let (_broker, credentials) = obtain_managed_credentials(&record, now)
            .await
            .expect("a sub-minimum cached grant must trigger a fresh mint");

        assert_eq!(
            credentials.grant_id.as_str(),
            "grant_01J",
            "the freshly minted grant must be returned, not the dead cached one"
        );
        let calls = fetch.calls();
        assert_eq!(calls.len(), 1, "exactly one mint request must be issued");
        assert!(
            calls[0]
                .url
                .ends_with("/pairings/pair_01J/broker-credentials"),
            "the call must be a broker credential mint: {calls:?}"
        );

        clear_cached_credentials(&record.local_host_id);
        let _ = super::super::store::delete_device_secret(&device_secret_key_id).await;
        drop(fetch);
        set_config("");
    }

    /// The in-memory grant cache still works: a cached grant whose service
    /// connect boundary is safely beyond the clock-skew allowance is reused
    /// without any service call.
    #[wasm_bindgen_test]
    async fn cached_grant_within_service_connect_boundary_is_reused() {
        set_config(r#"{"baseUrl":"https://tycode.dev/api/tyde/mobile/v1","provider":"google"}"#);
        let fetch = install_fetch_mock(Vec::new());
        // The device secret is intentionally absent: a cache hit must never
        // touch the keystore or the service.
        let record = reconnect_record(
            "reconnect-fresh-grant",
            mobile_shell_types::KeychainSecretId("unused-device-secret".to_owned()),
        );
        let now = 1_000_000_000_u64;
        cache_credentials(
            &record.local_host_id,
            cached_test_grant(
                "grant_cached_fresh",
                now + CREDENTIAL_CLOCK_SKEW_ALLOWANCE_MS + 60_000,
                now + 900_000,
            ),
        );

        let (_broker, credentials) = obtain_managed_credentials(&record, now)
            .await
            .expect("a still-valid cached grant must be reused");

        assert_eq!(credentials.grant_id.as_str(), "grant_cached_fresh");
        assert!(
            fetch.calls().is_empty(),
            "no service request may be issued for a still-valid cached grant"
        );

        clear_cached_credentials(&record.local_host_id);
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
                    "connect_valid_until_ms":4102444500000,
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
