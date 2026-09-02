//! Web Push subscription management for the PWA.
//!
//! The VAPID key pair is generated here, on the device, and its private half is
//! handed to every paired host over the already-encrypted pairing channel. That
//! is what makes multi-host work: a browser allows one push subscription per
//! service worker registration, bound to a single application server key, so
//! per-host keys would let only one host ever notify this device.
//!
//! Nothing here silently degrades. If notifications are unavailable — no
//! service worker, no Push API, permission denied — the caller gets an error
//! describing which, and the UI says so.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::SecretKey;
use p256::elliptic_curve::rand_core::OsRng;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use protocol::{
    MobilePushSubscription, PushAuthSecret, PushEndpointUrl, PushPublicKey, VapidPrivateKey,
    VapidPublicKey,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    NotificationPermission, PushEncryptionKeyName, PushSubscription, PushSubscriptionOptionsInit,
    ServiceWorkerRegistration,
};

use crate::bridge::web::idb;

/// Device-global, not per-host: every paired host signs with the same key so
/// they can share this device's single subscription.
const VAPID_KEY_ID: &str = "vapid-keypair-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVapidKeyPair {
    public: String,
    private: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushAvailability {
    /// The browser exposes the Push API and permission has been granted.
    Granted,
    /// The Push API exists but the user has not been asked yet.
    Prompt,
    /// The user refused. Only they can undo this, in browser settings.
    Denied,
    /// No Push API. On iOS this is what a Safari tab reports; the app must be
    /// installed to the Home Screen first.
    Unsupported,
}

pub fn availability() -> PushAvailability {
    let Some(window) = web_sys::window() else {
        return PushAvailability::Unsupported;
    };
    if !js_sys::Reflect::has(&window, &"PushManager".into()).unwrap_or(false) {
        return PushAvailability::Unsupported;
    }
    if window.navigator().service_worker().is_undefined() {
        return PushAvailability::Unsupported;
    }
    match web_sys::Notification::permission() {
        NotificationPermission::Granted => PushAvailability::Granted,
        NotificationPermission::Denied => PushAvailability::Denied,
        _ => PushAvailability::Prompt,
    }
}

async fn registration() -> Result<ServiceWorkerRegistration, String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_owned())?;
    let ready = window
        .navigator()
        .service_worker()
        .ready()
        .map_err(|error| format!("service worker unavailable: {error:?}"))?;
    let value = JsFuture::from(ready)
        .await
        .map_err(|error| format!("service worker registration failed: {error:?}"))?;
    value
        .dyn_into::<ServiceWorkerRegistration>()
        .map_err(|_| "service worker ready did not yield a registration".to_owned())
}

async fn load_or_create_keypair() -> Result<StoredVapidKeyPair, String> {
    if let Some(raw) = idb::get(idb::STORE_PSK, VAPID_KEY_ID).await? {
        return serde_json::from_str(&raw)
            .map_err(|error| format!("stored VAPID key pair is unreadable: {error}"));
    }

    let secret = SecretKey::random(&mut OsRng);
    let point = secret.public_key().to_encoded_point(false);
    let keypair = StoredVapidKeyPair {
        public: URL_SAFE_NO_PAD.encode(point.as_bytes()),
        private: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
    };
    let encoded = serde_json::to_string(&keypair)
        .map_err(|error| format!("failed to encode VAPID key pair: {error}"))?;
    idb::put(idb::STORE_PSK, VAPID_KEY_ID, &encoded).await?;
    Ok(keypair)
}

fn subscription_key(
    subscription: &PushSubscription,
    name: PushEncryptionKeyName,
    field: &str,
) -> Result<String, String> {
    let buffer = subscription
        .get_key(name)
        .map_err(|error| format!("push subscription {field} unavailable: {error:?}"))?
        .ok_or_else(|| format!("push subscription has no {field}"))?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

async fn to_protocol(
    subscription: &PushSubscription,
    keypair: &StoredVapidKeyPair,
) -> Result<MobilePushSubscription, String> {
    Ok(MobilePushSubscription {
        endpoint: PushEndpointUrl(subscription.endpoint()),
        p256dh: PushPublicKey(subscription_key(
            subscription,
            PushEncryptionKeyName::P256dh,
            "p256dh",
        )?),
        auth: PushAuthSecret(subscription_key(
            subscription,
            PushEncryptionKeyName::Auth,
            "auth",
        )?),
        vapid_public_key: VapidPublicKey(keypair.public.clone()),
        vapid_private_key: VapidPrivateKey(keypair.private.clone()),
    })
}

/// Whether this device currently holds a browser push subscription. Permission
/// alone is not enough: the browser can drop a subscription while the
/// permission stays granted, and then no host can reach this device until it
/// subscribes again.
pub async fn subscribed() -> Result<bool, String> {
    if availability() != PushAvailability::Granted {
        return Ok(false);
    }
    let registration = registration().await?;
    let existing = JsFuture::from(
        registration
            .push_manager()
            .map_err(|error| format!("push manager unavailable: {error:?}"))?
            .get_subscription()
            .map_err(|error| format!("failed to read push subscription: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("failed to read push subscription: {error:?}"))?;
    Ok(!existing.is_null() && !existing.is_undefined())
}

/// The subscription this device already holds, if any. Read on every connect so
/// a rotated subscription reaches the host without depending on
/// `pushsubscriptionchange`, which iOS does not reliably fire.
pub async fn current_subscription() -> Result<Option<MobilePushSubscription>, String> {
    if availability() != PushAvailability::Granted {
        return Ok(None);
    }
    let registration = registration().await?;
    let existing = JsFuture::from(
        registration
            .push_manager()
            .map_err(|error| format!("push manager unavailable: {error:?}"))?
            .get_subscription()
            .map_err(|error| format!("failed to read push subscription: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("failed to read push subscription: {error:?}"))?;

    if existing.is_null() || existing.is_undefined() {
        return Ok(None);
    }
    let subscription = existing
        .dyn_into::<PushSubscription>()
        .map_err(|_| "unexpected push subscription value".to_owned())?;
    let keypair = load_or_create_keypair().await?;
    to_protocol(&subscription, &keypair).await.map(Some)
}

/// Requests permission if needed and subscribes. Must be called from a user
/// gesture: browsers reject a permission prompt raised any other way.
pub async fn enable() -> Result<MobilePushSubscription, String> {
    match availability() {
        PushAvailability::Unsupported => {
            return Err(
                "This browser cannot receive notifications. On iPhone, add Tyde to your \
                 Home Screen first."
                    .to_owned(),
            );
        }
        PushAvailability::Denied => {
            return Err(
                "Notifications are blocked for this site. Re-enable them in browser settings."
                    .to_owned(),
            );
        }
        PushAvailability::Prompt => {
            let request = web_sys::Notification::request_permission()
                .map_err(|error| format!("could not request notification permission: {error:?}"))?;
            JsFuture::from(request)
                .await
                .map_err(|error| format!("notification permission request failed: {error:?}"))?;
            if availability() != PushAvailability::Granted {
                return Err("Notification permission was not granted.".to_owned());
            }
        }
        PushAvailability::Granted => {}
    }

    let keypair = load_or_create_keypair().await?;
    let registration = registration().await?;
    let manager = registration
        .push_manager()
        .map_err(|error| format!("push manager unavailable: {error:?}"))?;

    // An existing subscription is bound to whatever key it was created with, so
    // reuse it rather than failing on a key mismatch.
    let existing = JsFuture::from(
        manager
            .get_subscription()
            .map_err(|error| format!("failed to read push subscription: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("failed to read push subscription: {error:?}"))?;
    if !existing.is_null() && !existing.is_undefined() {
        let subscription = existing
            .dyn_into::<PushSubscription>()
            .map_err(|_| "unexpected push subscription value".to_owned())?;
        return to_protocol(&subscription, &keypair).await;
    }

    let application_server_key = URL_SAFE_NO_PAD
        .decode(&keypair.public)
        .map_err(|error| format!("stored VAPID public key is unreadable: {error}"))?;
    let key_array = js_sys::Uint8Array::from(application_server_key.as_slice());
    let options = PushSubscriptionOptionsInit::new();
    // Required: a silent push is not permitted, and abusing it costs the
    // permission outright.
    options.set_user_visible_only(true);
    options.set_application_server_key(&key_array);

    let subscribed = JsFuture::from(
        manager
            .subscribe_with_options(&options)
            .map_err(|error| format!("push subscribe failed: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("push subscribe failed: {error:?}"))?;
    let subscription = subscribed
        .dyn_into::<PushSubscription>()
        .map_err(|_| "push subscribe did not yield a subscription".to_owned())?;
    to_protocol(&subscription, &keypair).await
}

/// Drops the browser subscription. The hosts are told separately; this only
/// undoes the local half.
pub async fn disable() -> Result<(), String> {
    let registration = registration().await?;
    let manager = registration
        .push_manager()
        .map_err(|error| format!("push manager unavailable: {error:?}"))?;
    let existing = JsFuture::from(
        manager
            .get_subscription()
            .map_err(|error| format!("failed to read push subscription: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("failed to read push subscription: {error:?}"))?;
    if existing.is_null() || existing.is_undefined() {
        return Ok(());
    }
    let subscription = existing
        .dyn_into::<PushSubscription>()
        .map_err(|_| "unexpected push subscription value".to_owned())?;
    JsFuture::from(
        subscription
            .unsubscribe()
            .map_err(|error| format!("push unsubscribe failed: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("push unsubscribe failed: {error:?}"))?;
    Ok(())
}
