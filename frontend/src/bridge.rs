use devtools_protocol::UiDebugRequestEvent;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

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
pub struct ConnectHostRequest {
    pub host_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendHostLineRequest {
    pub host_id: String,
    pub line: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLineEvent {
    pub host_id: String,
    pub line: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDisconnectedEvent {
    pub host_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostErrorEvent {
    pub host_id: String,
    pub message: String,
}

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

pub async fn connect_host(request: ConnectHostRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("connect_host", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn send_host_line(request: SendHostLineRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("send_host_line", args)
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
// Each returns an UnlistenHandle that keeps the closure and JS unlisten callback alive.
// In this desktop app, listeners live for the entire app lifetime and are intentionally
// leaked via `mem::forget` in app.rs — the unlisten callback is never invoked.

pub struct UnlistenHandle {
    _closure: Closure<dyn Fn(JsValue)>,
    _unlisten: JsValue,
}

pub async fn listen_host_line(
    callback: impl Fn(HostLineEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://host-line", move |val: JsValue| {
        let event: TauriEvent<HostLineEvent> = match serde_wasm_bindgen::from_value(val) {
            Ok(e) => e,
            Err(e) => {
                log::error!("failed to parse host-line event: {e:?}");
                return;
            }
        };
        callback(event.payload);
    })
    .await
}

pub async fn listen_host_disconnected(
    callback: impl Fn(HostDisconnectedEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://host-disconnected", move |val: JsValue| {
        let event: TauriEvent<HostDisconnectedEvent> = match serde_wasm_bindgen::from_value(val) {
            Ok(e) => e,
            Err(e) => {
                log::error!("failed to parse host-disconnected event: {e:?}");
                return;
            }
        };
        callback(event.payload);
    })
    .await
}

pub async fn listen_host_error(
    callback: impl Fn(HostErrorEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://host-error", move |val: JsValue| {
        let event: TauriEvent<HostErrorEvent> = match serde_wasm_bindgen::from_value(val) {
            Ok(e) => e,
            Err(e) => {
                log::error!("failed to parse host-error event: {e:?}");
                return;
            }
        };
        callback(event.payload);
    })
    .await
}

pub async fn listen_ui_debug_request(
    callback: impl Fn(UiDebugRequestEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event("tyde://ui-debug-request", move |val: JsValue| {
        let event: TauriEvent<UiDebugRequestEvent> = match serde_wasm_bindgen::from_value(val) {
            Ok(e) => e,
            Err(e) => {
                log::error!("failed to parse ui-debug-request event: {e:?}");
                return;
            }
        };
        callback(event.payload);
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
        _unlisten: unlisten,
    })
}
