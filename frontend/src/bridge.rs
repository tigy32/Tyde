use devtools_protocol::UiDebugRequestEvent;
use js_sys::Function;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

// Host-config types are defined once in the `host-config` crate and shared
// verbatim with the Tauri shell. Re-export them for downstream modules.
pub use host_config::{
    ConfiguredHost, ConfiguredHostStore, HostDisconnectedEvent, HostErrorEvent, HostIdRequest,
    HostLifecycleEvent, HostLineEvent, HostTransportConfig, RemoteHostLifecycleConfig,
    RemoteHostLifecycleSnapshot, RemoteHostLifecycleStatus, RemoteHostLifecycleStep,
    RemoteTydeRunningState, SendHostLineRequest, SetSelectedHostRequest, TydeReleaseTarget,
    UpsertConfiguredHostRequest,
};

// --- Tauri JS bindings ---

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = "invoke", catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

// --- Request/response types (owned by frontend, matching tauri-shell's bridge) ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitUiDebugResponseRequest {
    pub request_id: String,
    pub response: devtools_protocol::UiDebugResponse,
}

// --- Tauri event wrapper (events arrive as { event, payload }) ---

#[derive(Deserialize)]
struct TauriEvent<T> {
    payload: T,
}

// --- Command invocations ---

pub async fn connect_host(request: HostIdRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("connect_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn disconnect_host(host_id: String) -> Result<(), String> {
    let args =
        serde_wasm_bindgen::to_value(&HostIdRequest { host_id }).map_err(|e| e.to_string())?;
    tauri_invoke("disconnect_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn list_configured_hosts() -> Result<ConfiguredHostStore, String> {
    let value = tauri_invoke("list_configured_hosts", JsValue::NULL)
        .await
        .map_err(|e| format!("{e:?}"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn upsert_configured_host(
    request: UpsertConfiguredHostRequest,
) -> Result<ConfiguredHostStore, String> {
    #[derive(Serialize)]
    struct Args {
        request: UpsertConfiguredHostRequest,
    }
    let args = serde_wasm_bindgen::to_value(&Args { request }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("upsert_configured_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn remove_configured_host(host_id: String) -> Result<ConfiguredHostStore, String> {
    let args =
        serde_wasm_bindgen::to_value(&HostIdRequest { host_id }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("remove_configured_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn set_selected_host(
    request: SetSelectedHostRequest,
) -> Result<ConfiguredHostStore, String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    let value = tauri_invoke("set_selected_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn send_host_line(request: SendHostLineRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("send_host_line", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn ensure_configured_host_ready(
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let args =
        serde_wasm_bindgen::to_value(&HostIdRequest { host_id }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("ensure_configured_host_ready", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn submit_feedback(feedback: String) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Args {
        feedback: String,
    }
    let args = serde_wasm_bindgen::to_value(&Args { feedback }).map_err(|e| e.to_string())?;
    tauri_invoke("submit_feedback", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn mark_ui_debug_ready() -> Result<(), String> {
    tauri_invoke("mark_ui_debug_ready", JsValue::NULL)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn submit_ui_debug_response(request: SubmitUiDebugResponseRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("submit_ui_debug_response", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

// --- Event listeners ---
// Each returns an UnlistenHandle that keeps the closure and JS unlisten callback alive
// until the app tears the listener down.

pub struct UnlistenHandle {
    _closure: Closure<dyn Fn(JsValue)>,
    unlisten: JsValue,
}

impl UnlistenHandle {
    pub fn remove(self) {
        if let Ok(callback) = self.unlisten.dyn_into::<Function>() {
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

pub async fn listen_host_lifecycle(
    callback: impl Fn(HostLifecycleEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-lifecycle",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<HostLifecycleEvent>>(
            val,
        ) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-lifecycle event: {error}"),
        },
    )
    .await
}

pub async fn listen_ui_debug_request(
    callback: impl Fn(UiDebugRequestEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://ui-debug-request",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<UiDebugRequestEvent>>(
            val,
        ) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse ui-debug-request event: {error}"),
        },
    )
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
