use devtools_protocol::UiDebugRequestEvent;
use js_sys::Function;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

// Host-config types are defined once in the `host-config` crate and shared
// verbatim with the Tauri shell. Re-export them for downstream modules.
pub use host_config::{
    ConfiguredHost, ConfiguredHostStore, HostDisconnectedEvent, HostErrorEvent, HostIdRequest,
    HostLifecycleEvent, HostLineEvent, HostRecoveryEvent, HostTransportConfig, HostWarningEvent,
    RemoteHostLifecycleConfig, RemoteHostLifecycleSnapshot, RemoteHostLifecycleStatus,
    RemoteHostLifecycleStep, RemoteTydeRunningState, SendHostLineRequest, SetSelectedHostRequest,
    UpsertConfiguredHostRequest,
};

// --- Tauri JS bindings ---

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = "invoke", catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(
        js_namespace = ["window", "__TAURI__", "event"],
        js_name = "listen",
        catch
    )]
    fn tauri_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> Result<JsValue, JsValue>;
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

const UNKNOWN_DESKTOP_HOST_ERROR: &str = "The desktop host returned an unknown error.";

fn tauri_error_message(command: &'static str, value: JsValue) -> String {
    if let Some(message) = value.as_string() {
        let message = message.trim();
        if !message.is_empty() {
            return message.to_owned();
        }
        log::warn!("Tauri command {command} rejected with blank_string");
        return UNKNOWN_DESKTOP_HOST_ERROR.to_owned();
    }

    let message = match js_sys::Reflect::get(&value, &JsValue::from_str("message")) {
        Ok(message) => message,
        Err(_) => {
            log::warn!("Tauri command {command} rejected with message_getter_failed");
            return UNKNOWN_DESKTOP_HOST_ERROR.to_owned();
        }
    };
    if let Some(message) = message.as_string() {
        let message = message.trim();
        if !message.is_empty() {
            return message.to_owned();
        }
    }

    let category = if value.is_null() {
        "null"
    } else if value.is_undefined() {
        "undefined"
    } else if value.is_object() {
        "object_without_string_message"
    } else {
        "non_string_primitive"
    };
    log::warn!("Tauri command {command} rejected with {category}");
    UNKNOWN_DESKTOP_HOST_ERROR.to_owned()
}

pub async fn connect_host(request: HostIdRequest) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&request).map_err(|e| e.to_string())?;
    tauri_invoke("connect_host", args)
        .await
        .map_err(|error| tauri_error_message("connect_host", error))?;
    Ok(())
}

pub async fn retry_host(host_id: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&HostIdRequest { host_id })
        .map_err(|error| error.to_string())?;
    tauri_invoke("retry_host", args)
        .await
        .map_err(|error| tauri_error_message("retry_host", error))?;
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SendHostFrameArgs<'a> {
    host_id: &'a str,
    envelope: &'a str,
    binary: &'a [u8],
}

pub async fn send_host_frame(host_id: &str, envelope: &str, binary: &[u8]) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SendHostFrameArgs {
        host_id,
        envelope,
        binary,
    })
    .map_err(|error| error.to_string())?;
    tauri_invoke("send_host_frame", args)
        .await
        .map_err(|error| tauri_error_message("send_host_frame", error))?;
    Ok(())
}

pub async fn voice_media_start(
    host_id: &str,
    generation: u64,
    input_only: bool,
    pending_acceptance: bool,
) -> Result<(), String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Args<'a> {
        host_id: &'a str,
        generation: u64,
        input_only: bool,
        pending_acceptance: bool,
    }
    let args = serde_wasm_bindgen::to_value(&Args {
        host_id,
        generation,
        input_only,
        pending_acceptance,
    })
    .map_err(|error| error.to_string())?;
    tauri_invoke("voice_media_start", args)
        .await
        .map_err(|error| tauri_error_message("voice_media_start", error))?;
    Ok(())
}

pub async fn voice_media_push_output(
    generation: u64,
    media_seq: u64,
    timestamp_samples_48k: u64,
    opus: &[u8],
) -> Result<(), String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Args<'a> {
        generation: u64,
        media_seq: u64,
        timestamp_samples_48k: u64,
        opus: &'a [u8],
    }
    let args = serde_wasm_bindgen::to_value(&Args {
        generation,
        media_seq,
        timestamp_samples_48k,
        opus,
    })
    .map_err(|error| error.to_string())?;
    tauri_invoke("voice_media_push_output", args)
        .await
        .map_err(|error| tauri_error_message("voice_media_push_output", error))?;
    Ok(())
}

pub async fn voice_media_flush_output(generation: u64) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args {
        generation: u64,
    }
    let args =
        serde_wasm_bindgen::to_value(&Args { generation }).map_err(|error| error.to_string())?;
    tauri_invoke("voice_media_flush_output", args)
        .await
        .map_err(|error| tauri_error_message("voice_media_flush_output", error))?;
    Ok(())
}

pub async fn voice_media_stop() -> Result<(), String> {
    tauri_invoke("voice_media_stop", JsValue::NULL)
        .await
        .map_err(|error| tauri_error_message("voice_media_stop", error))?;
    Ok(())
}

pub async fn voice_media_supported() -> Result<bool, String> {
    let value = tauri_invoke("voice_media_supported", JsValue::NULL)
        .await
        .map_err(|error| tauri_error_message("voice_media_supported", error))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| error.to_string())
}

pub async fn ensure_configured_host_ready(
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let args =
        serde_wasm_bindgen::to_value(&HostIdRequest { host_id }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("ensure_configured_host_ready", args)
        .await
        .map_err(|error| tauri_error_message("ensure_configured_host_ready", error))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

/// Force a managed remote host through a stop + relaunch/upgrade of the exact
/// target release, bypassing the normal "serve as-is" decision. Used by the
/// Phase 2 safety net when a managed host rejects the handshake with
/// `IncompatibleProtocol` despite a compatible-looking probe. Contacts GitHub
/// only when the target binary is missing; surfaces errors verbatim (no
/// fallback to an older release).
pub async fn force_upgrade_managed_host(
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let args =
        serde_wasm_bindgen::to_value(&HostIdRequest { host_id }).map_err(|e| e.to_string())?;
    let value = tauri_invoke("force_upgrade_managed_host", args)
        .await
        .map_err(|error| tauri_error_message("force_upgrade_managed_host", error))?;
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

pub async fn open_external_url(url: String) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Args {
        url: String,
    }
    let args = serde_wasm_bindgen::to_value(&Args { url }).map_err(|e| e.to_string())?;
    tauri_invoke("open_external_url", args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

/// Show a native OK/Cancel confirmation dialog via tauri-plugin-dialog.
///
/// Use this instead of `web_sys::Window::confirm_with_message` — the WKWebView
/// `window.confirm()` API is silently no-op'd inside Tauri, so this is the
/// only way to get a working confirmation prompt in the desktop/mobile shell.
pub async fn confirm_dialog(title: &str, message: &str) -> bool {
    #[derive(Serialize)]
    struct Args<'a> {
        title: &'a str,
        message: &'a str,
        kind: &'a str,
        buttons: &'a str,
    }
    let args = match serde_wasm_bindgen::to_value(&Args {
        title,
        message,
        kind: "warning",
        buttons: "OkCancel",
    }) {
        Ok(v) => v,
        Err(e) => {
            log::error!("confirm_dialog: failed to serialize args: {e}");
            return false;
        }
    };
    match tauri_invoke("plugin:dialog|message", args).await {
        Ok(v) => v.as_string().as_deref() == Some("Ok"),
        Err(e) => {
            log::error!("confirm_dialog: plugin:dialog|message failed: {e:?}");
            false
        }
    }
}

pub async fn message_dialog(title: &str, message: &str) {
    #[derive(Serialize)]
    struct Args<'a> {
        title: &'a str,
        message: &'a str,
        kind: &'a str,
        buttons: &'a str,
    }
    let args = match serde_wasm_bindgen::to_value(&Args {
        title,
        message,
        kind: "error",
        buttons: "Ok",
    }) {
        Ok(value) => value,
        Err(error) => {
            log::error!("message_dialog: failed to serialize args: {error}");
            return;
        }
    };
    if let Err(error) = tauri_invoke("plugin:dialog|message", args).await {
        log::error!("message_dialog: plugin:dialog|message failed: {error:?}");
    }
}

pub async fn mark_ui_debug_ready() -> Result<(), String> {
    tauri_invoke("mark_ui_debug_ready", JsValue::NULL)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn mark_frontend_ready() -> Result<(), String> {
    tauri_invoke("mark_frontend_ready", JsValue::NULL)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn report_frontend_lifecycle(event: &'static str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args {
        event: &'static str,
    }

    let args = serde_wasm_bindgen::to_value(&Args { event }).map_err(|e| e.to_string())?;
    tauri_invoke("report_frontend_lifecycle", args)
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

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostVoiceFrameEvent {
    pub host_id: String,
    pub envelope: String,
    pub opus: Vec<u8>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOpusPacketEvent {
    pub generation: u64,
    pub media_seq: u64,
    pub timestamp_samples_48k: u64,
    pub opus: Vec<u8>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceMediaStateEvent {
    pub generation: u64,
    pub state: String,
    pub native_aec: bool,
}

pub async fn listen_host_voice_frame(
    callback: impl Fn(HostVoiceFrameEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-voice-frame",
        move |value| match serde_wasm_bindgen::from_value::<TauriEvent<HostVoiceFrameEvent>>(value)
        {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host voice frame: {error}"),
        },
    )
    .await
}

pub async fn listen_voice_opus_packet(
    callback: impl Fn(VoiceOpusPacketEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://voice-opus-packet",
        move |value| match decode_voice_opus_packet_event(value) {
            Ok(event) => callback(event),
            Err(error) => log::error!("failed to parse native Opus packet: {error}"),
        },
    )
    .await
}

pub(crate) fn decode_voice_opus_packet_event(
    value: JsValue,
) -> Result<VoiceOpusPacketEvent, String> {
    let event = serde_wasm_bindgen::from_value::<TauriEvent<VoiceOpusPacketEvent>>(value)
        .map_err(|error| error.to_string())?;
    Ok(event.payload)
}

pub async fn listen_voice_media_state(
    callback: impl Fn(VoiceMediaStateEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://voice-media-state",
        move |value| match serde_wasm_bindgen::from_value::<TauriEvent<VoiceMediaStateEvent>>(value)
        {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse native voice media state: {error}"),
        },
    )
    .await
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

pub async fn listen_host_recovery(
    callback: impl Fn(HostRecoveryEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-recovery",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<HostRecoveryEvent>>(
            val,
        ) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-recovery event: {error}"),
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

pub async fn listen_host_warning(
    callback: impl Fn(HostWarningEvent) + 'static,
) -> Result<UnlistenHandle, String> {
    listen_event(
        "tyde://host-warning",
        move |val: JsValue| match serde_wasm_bindgen::from_value::<TauriEvent<HostWarningEvent>>(
            val,
        ) {
            Ok(event) => callback(event.payload),
            Err(error) => log::error!("failed to parse host-warning event: {error}"),
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
    let promise = tauri_listen(event_name, &closure)
        .map_err(|_| format!("failed to register {event_name}: Tauri event API unavailable"))?
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| {
            format!("failed to register {event_name}: listener did not return a promise")
        })?;
    let unlisten = JsFuture::from(promise)
        .await
        .map_err(|_| format!("failed to register {event_name}: listener promise rejected"))?;
    Ok(UnlistenHandle {
        _closure: closure,
        unlisten,
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    struct WindowPropertyGuard {
        name: &'static str,
        existed: bool,
        value: JsValue,
    }

    impl WindowPropertyGuard {
        fn remove(name: &'static str) -> Self {
            let window = web_sys::window().expect("window");
            let key = JsValue::from_str(name);
            let existed = js_sys::Reflect::has(&window, &key).unwrap_or(false);
            let value = js_sys::Reflect::get(&window, &key).unwrap_or(JsValue::UNDEFINED);
            let target: &js_sys::Object = window.unchecked_ref();
            js_sys::Reflect::delete_property(target, &key).expect("remove window property");
            Self {
                name,
                existed,
                value,
            }
        }
    }

    impl Drop for WindowPropertyGuard {
        fn drop(&mut self) {
            let window = web_sys::window().expect("window");
            let key = JsValue::from_str(self.name);
            if self.existed {
                let _ = js_sys::Reflect::set(&window, &key, &self.value);
            } else {
                let target: &js_sys::Object = window.unchecked_ref();
                let _ = js_sys::Reflect::delete_property(target, &key);
            }
        }
    }

    #[wasm_bindgen_test]
    async fn absent_tauri_event_runtime_rejects_listener_without_throwing() {
        let _guard = WindowPropertyGuard::remove("__TAURI__");
        let error = match listen_voice_opus_packet(|_| {}).await {
            Ok(_) => panic!("listener registration unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "failed to register tyde://voice-opus-packet: Tauri event API unavailable"
        );
    }

    #[wasm_bindgen_test]
    fn host_rejection_uses_plain_string_or_exception_safe_message() {
        assert_eq!(
            tauri_error_message("connect_host", JsValue::from_str("  SSH unavailable  ")),
            "SSH unavailable"
        );

        let error = js_sys::Error::new("SSH authentication failed");
        assert_eq!(
            tauri_error_message("connect_host", error.into()),
            "SSH authentication failed"
        );

        let throwing =
            js_sys::eval("new Proxy({}, { get() { throw new Error('private getter detail'); } })")
                .expect("construct throwing error proxy");
        assert_eq!(
            tauri_error_message("connect_host", throwing),
            UNKNOWN_DESKTOP_HOST_ERROR,
            "a throwing message getter must not escape or expose its exception"
        );
    }

    #[wasm_bindgen_test]
    fn host_rejection_never_serializes_unknown_values() {
        let unknown = js_sys::Object::new();
        js_sys::Reflect::set(
            &unknown,
            &JsValue::from_str("credential"),
            &JsValue::from_str("sentinel-secret"),
        )
        .expect("seed unrelated private field");

        for value in [
            JsValue::NULL,
            JsValue::UNDEFINED,
            JsValue::from_f64(42.0),
            unknown.into(),
        ] {
            let message = tauri_error_message("ensure_configured_host_ready", value);
            assert_eq!(message, UNKNOWN_DESKTOP_HOST_ERROR);
            assert!(!message.contains("sentinel-secret"));
            assert!(!message.contains("JsValue("));
        }

        assert_eq!(
            tauri_error_message("connect_host", JsValue::from_str("   ")),
            UNKNOWN_DESKTOP_HOST_ERROR
        );
    }
}
