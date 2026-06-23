//! Minimal async IndexedDB key/value helper for the browser (PWA) backend.
//!
//! The Tauri shell persists paired hosts + PSKs in the iOS Keychain / app-data
//! dir. In the browser there is no Keychain, so the web backend persists the
//! same data in IndexedDB. This module is a thin promise-style wrapper over the
//! event-based `web_sys` IndexedDB API: open the database (creating the object
//! stores on first run) and `get` / `put` / `delete` string values by key.
//!
//! Values are stored as strings (JSON for records, base64 for PSK bytes) so the
//! object stores never need structured-clone-specific handling. A later phase
//! (see `docs/plans/mobile-web-pwa.md` → "PSK storage") swaps the PSK store to a
//! non-extractable WebCrypto `CryptoKey`; that change is localized to
//! [`super::store`] and does not touch this generic helper.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const DB_NAME: &str = "tyde-mobile";
const DB_VERSION: u32 = 1;

pub const STORE_HOSTS: &str = "paired_hosts";
pub const STORE_PSK: &str = "psk";

fn jserr(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

async fn await_request<R: AsRef<IdbRequest>>(req: R) -> Result<JsValue, String> {
    let req: &IdbRequest = req.as_ref();
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<JsValue, String>>();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let req_succ = req.clone();
    let tx_succ = tx.clone();
    let onsuccess = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = tx_succ.borrow_mut().take() {
            let _ = tx.send(req_succ.result().map_err(jserr));
        }
    });
    let req_err = req.clone();
    let tx_err = tx.clone();
    let onerror = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = tx_err.borrow_mut().take() {
            let message = req_err
                .error()
                .ok()
                .flatten()
                .map(|exception| exception.message())
                .unwrap_or_else(|| "indexeddb request failed".to_owned());
            let _ = tx.send(Err(message));
        }
    });

    req.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    req.set_onerror(Some(onerror.as_ref().unchecked_ref()));

    let outcome = rx
        .await
        .map_err(|_| "indexeddb request was dropped before completing".to_owned())?;
    // Keep the closures alive until the request settled.
    drop(onsuccess);
    drop(onerror);
    outcome
}

async fn open_db() -> Result<IdbDatabase, String> {
    let window = web_sys::window().ok_or("no window for indexeddb")?;
    let factory = window
        .indexed_db()
        .map_err(jserr)?
        .ok_or("indexedDB is unavailable in this browser")?;
    let open_req: IdbOpenDbRequest = factory.open_with_u32(DB_NAME, DB_VERSION).map_err(jserr)?;

    let onupgrade = Closure::<dyn FnMut(web_sys::IdbVersionChangeEvent)>::new(
        move |event: web_sys::IdbVersionChangeEvent| {
            let Some(target) = event.target() else {
                return;
            };
            let Ok(req) = target.dyn_into::<IdbOpenDbRequest>() else {
                return;
            };
            let Ok(result) = req.result() else {
                return;
            };
            let Ok(db) = result.dyn_into::<IdbDatabase>() else {
                return;
            };
            let names = db.object_store_names();
            if !names.contains(STORE_HOSTS) {
                let _ = db.create_object_store(STORE_HOSTS);
            }
            if !names.contains(STORE_PSK) {
                let _ = db.create_object_store(STORE_PSK);
            }
        },
    );
    open_req.set_onupgradeneeded(Some(onupgrade.as_ref().unchecked_ref()));

    let result = await_request(open_req).await?;
    drop(onupgrade);
    result
        .dyn_into::<IdbDatabase>()
        .map_err(|_| "indexeddb open did not return a database".to_owned())
}

pub async fn get(store: &str, key: &str) -> Result<Option<String>, String> {
    let db = open_db().await?;
    let tx = db
        .transaction_with_str_and_mode(store, IdbTransactionMode::Readonly)
        .map_err(jserr)?;
    let object_store = tx.object_store(store).map_err(jserr)?;
    let request = object_store.get(&JsValue::from_str(key)).map_err(jserr)?;
    let value = await_request(request).await?;
    db.close();
    if value.is_undefined() || value.is_null() {
        Ok(None)
    } else {
        value
            .as_string()
            .map(Some)
            .ok_or_else(|| "indexeddb value was not a string".to_owned())
    }
}

pub async fn put(store: &str, key: &str, value: &str) -> Result<(), String> {
    let db = open_db().await?;
    let tx = db
        .transaction_with_str_and_mode(store, IdbTransactionMode::Readwrite)
        .map_err(jserr)?;
    let object_store = tx.object_store(store).map_err(jserr)?;
    let request = object_store
        .put_with_key(&JsValue::from_str(value), &JsValue::from_str(key))
        .map_err(jserr)?;
    await_request(request).await?;
    db.close();
    Ok(())
}

pub async fn delete(store: &str, key: &str) -> Result<(), String> {
    let db = open_db().await?;
    let tx = db
        .transaction_with_str_and_mode(store, IdbTransactionMode::Readwrite)
        .map_err(jserr)?;
    let object_store = tx.object_store(store).map_err(jserr)?;
    let request = object_store
        .delete(&JsValue::from_str(key))
        .map_err(jserr)?;
    await_request(request).await?;
    db.close();
    Ok(())
}
