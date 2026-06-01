use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

pub use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
pub use mobile_shell_types::{
    KnownConnectionInstance, PairedHostConnectionStatusEvent, PairedHostsChangedEvent,
};

use crate::state::{LocalHostId, MobilePairingPreview, MobileShellError, PairedHostSummary};

// --- Tauri JS bindings ---

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = "invoke", catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

// --- Tauri event wrapper ---

#[derive(Deserialize)]
struct TauriEvent<T> {
    payload: T,
}

// --- Mobile-shell command request/response shapes ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingUriRequest<'a> {
    pub qr_uri: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalHostIdRequest<'a> {
    local_host_id: &'a LocalHostId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AutoConnectRequest<'a> {
    local_host_id: &'a LocalHostId,
    auto_connect: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SendHostLineRequest<'a> {
    local_host_id: &'a LocalHostId,
    line: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AckHostLineRequest<'a> {
    local_host_id: &'a LocalHostId,
    delivery_id: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FrontendAttachedRequest<'a> {
    known_connection_instance_ids: &'a [KnownConnectionInstance],
}

// --- Command invocations ---

async fn invoke_unit(cmd: &str, args: JsValue) -> Result<(), String> {
    tauri_invoke(cmd, args).await.map_err(format_invoke_error)?;
    Ok(())
}

fn format_invoke_error(error: JsValue) -> String {
    // Tauri returns command errors as JsValue (typically a serde-encoded object
    // for typed Result<_, MobileCommandError>). Extract a useful string.
    if let Ok(parsed) = serde_wasm_bindgen::from_value::<MobileShellError>(error.clone()) {
        return format!("{:?}: {}", parsed.code, parsed.message);
    }
    if let Some(s) = error.as_string() {
        return s;
    }
    format!("{error:?}")
}

pub async fn list_paired_hosts() -> Result<Vec<PairedHostSummary>, String> {
    let value = tauri_invoke("list_paired_hosts", JsValue::NULL)
        .await
        .map_err(format_invoke_error)?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("decode failed: {error}"))
}

pub async fn list_paired_host_connection_statuses()
-> Result<Vec<PairedHostConnectionStatusEvent>, String> {
    let value = tauri_invoke("list_paired_host_connection_statuses", JsValue::NULL)
        .await
        .map_err(format_invoke_error)?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("decode failed: {error}"))
}

pub async fn list_pending_host_lines() -> Result<Vec<HostLineEvent>, String> {
    let value = tauri_invoke("list_pending_host_lines", JsValue::NULL)
        .await
        .map_err(format_invoke_error)?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("decode failed: {error}"))
}

pub async fn preview_pairing_uri(qr_uri: &str) -> Result<MobilePairingPreview, String> {
    let args =
        serde_wasm_bindgen::to_value(&PairingUriRequest { qr_uri }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("preview_pairing_uri", args)
        .await
        .map_err(format_invoke_error)?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("decode failed: {error}"))
}

pub async fn start_pairing(qr_uri: &str) -> Result<(), String> {
    let args =
        serde_wasm_bindgen::to_value(&PairingUriRequest { qr_uri }).map_err(|e| e.to_string())?;
    invoke_unit("start_pairing", args).await
}

pub async fn connect_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&LocalHostIdRequest { local_host_id })
        .map_err(|e| e.to_string())?;
    invoke_unit("connect_paired_host", args).await
}

pub async fn disconnect_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&LocalHostIdRequest { local_host_id })
        .map_err(|e| e.to_string())?;
    invoke_unit("disconnect_paired_host", args).await
}

pub async fn forget_paired_host(local_host_id: &LocalHostId) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&LocalHostIdRequest { local_host_id })
        .map_err(|e| e.to_string())?;
    invoke_unit("forget_paired_host", args).await
}

pub async fn set_paired_host_auto_connect(
    local_host_id: &LocalHostId,
    auto_connect: bool,
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&AutoConnectRequest {
        local_host_id,
        auto_connect,
    })
    .map_err(|e| e.to_string())?;
    invoke_unit("set_paired_host_auto_connect", args).await
}

pub async fn send_host_line(local_host_id: &LocalHostId, line: &str) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SendHostLineRequest {
        local_host_id,
        line,
    })
    .map_err(|e| e.to_string())?;
    invoke_unit("send_host_line", args).await
}

pub async fn ack_host_line(local_host_id: &LocalHostId, delivery_id: u64) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&AckHostLineRequest {
        local_host_id,
        delivery_id,
    })
    .map_err(|e| e.to_string())?;
    invoke_unit("ack_host_line", args).await
}

pub async fn frontend_attached(
    known_connection_instance_ids: &[KnownConnectionInstance],
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&FrontendAttachedRequest {
        known_connection_instance_ids,
    })
    .map_err(|e| e.to_string())?;
    invoke_unit("frontend_attached", args).await
}

// --- Barcode scanner ---

#[derive(Deserialize)]
pub struct BarcodeScanResult {
    pub content: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub format: Option<String>,
}

#[derive(Serialize)]
struct ScanArgs<'a> {
    #[serde(rename = "windowed")]
    windowed: bool,
    formats: &'a [&'a str],
}

#[derive(Deserialize)]
struct PermissionState {
    camera: String,
}

pub async fn scan_qr() -> Result<BarcodeScanResult, String> {
    let args = serde_wasm_bindgen::to_value(&ScanArgs {
        windowed: false,
        formats: &["QR_CODE"],
    })
    .map_err(|e| e.to_string())?;
    let value = tauri_invoke("plugin:barcode-scanner|scan", args)
        .await
        .map_err(format_invoke_error)?;
    serde_wasm_bindgen::from_value(value).map_err(|e| format!("decode scan result failed: {e}"))
}

async fn check_camera_permission() -> Result<String, String> {
    let value = tauri_invoke("plugin:barcode-scanner|check_permissions", JsValue::NULL)
        .await
        .map_err(format_invoke_error)?;
    let parsed: PermissionState =
        serde_wasm_bindgen::from_value(value).map_err(|e| format!("decode perms failed: {e}"))?;
    Ok(parsed.camera)
}

async fn request_camera_permission() -> Result<String, String> {
    let value = tauri_invoke("plugin:barcode-scanner|request_permissions", JsValue::NULL)
        .await
        .map_err(format_invoke_error)?;
    let parsed: PermissionState =
        serde_wasm_bindgen::from_value(value).map_err(|e| format!("decode perms failed: {e}"))?;
    Ok(parsed.camera)
}

pub async fn ensure_camera_permission() -> Result<(), String> {
    let state = check_camera_permission().await?;
    if state == "granted" {
        return Ok(());
    }
    let requested = request_camera_permission().await?;
    if requested == "granted" {
        Ok(())
    } else {
        Err(format!(
            "Camera permission was {requested}. Enable it in iOS Settings → Tyde → Camera."
        ))
    }
}

// --- Diagnostic logging (visible in Rust stderr) ---

#[allow(dead_code)]
pub async fn wasm_log(level: &str, message: &str) {
    #[derive(serde::Serialize)]
    struct Args<'a> {
        level: &'a str,
        message: &'a str,
    }
    if let Ok(args) = serde_wasm_bindgen::to_value(&Args { level, message }) {
        let _ = tauri_invoke("wasm_log", args).await;
    }
}

// --- Event listeners ---

pub struct UnlistenHandle {
    _closure: Closure<dyn Fn(JsValue)>,
    #[allow(dead_code)]
    unlisten: JsValue,
}

impl UnlistenHandle {
    #[allow(dead_code)]
    pub fn remove(self) {
        if let Ok(callback) = self.unlisten.dyn_into::<js_sys::Function>() {
            let _ = callback.call0(&JsValue::NULL);
        }
    }
}

pub async fn listen_host_line(
    callback: impl Fn(HostLineEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-line",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<HostLineEvent>>(val) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-line event: {error}"),
        },
    )
    .await
}

pub async fn listen_host_disconnected(
    callback: impl Fn(HostDisconnectedEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://host-disconnected", move |val: JsValue| {
        match serde_wasm_bindgen::from_value::<TauriEvent<HostDisconnectedEvent>>(val) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-disconnected event: {error}"),
        }
    })
    .await
}

pub async fn listen_host_error(
    callback: impl Fn(HostErrorEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-error",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<HostErrorEvent>>(val)
        {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-error event: {error}"),
        },
    )
    .await
}

pub async fn listen_paired_hosts_changed(
    callback: impl Fn(PairedHostsChangedEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://paired-hosts-changed", move |val: JsValue| {
        match serde_wasm_bindgen::from_value::<TauriEvent<PairedHostsChangedEvent>>(val) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse paired-hosts-changed event: {error}"),
        }
    })
    .await
}

pub async fn listen_paired_host_connection_status(
    callback: impl Fn(PairedHostConnectionStatusEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://paired-host-connection-status",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<
            TauriEvent<PairedHostConnectionStatusEvent>,
        >(val)
        {
            Ok(event) => callback(event.payload),
            Err(error) => {
                log::error!("failed to parse paired-host-connection-status event: {error}")
            }
        },
    )
    .await
}

pub async fn listen_mobile_shell_error(
    callback: impl Fn(MobileShellError) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://mobile-shell-error", move |val: JsValue| {
        match serde_wasm_bindgen::from_value::<TauriEvent<MobileShellError>>(val) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse mobile-shell-error event: {error}"),
        }
    })
    .await
}

async fn listen_event(
    event_name: &str,
    handler: impl Fn(JsValue) + 'static,
) -> Result<UnlistenHandle, String> {
    let closure = Closure::new(handler);
    let promise = tauri_listen(event_name, &closure);
    let unlisten = JsFuture::from(promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(UnlistenHandle {
        _closure: closure,
        unlisten,
    })
}
