mod bridge;
mod dev_host;
mod devtools;
mod host_bridge_uds;
mod host_stdio;
mod host_store;
mod host_uds;
mod logging;
mod router;

use std::sync::Arc;

use devtools_protocol::UiDebugResponseSubmission;
use host_store::{ConfiguredHostStore, HostStore, UpsertConfiguredHostRequest};
use router::ProxyRouterHandle;
use tauri::Manager;

#[cfg(target_os = "macos")]
mod macos_webview_defaults {
    use std::ffi::CString;

    use core_foundation_sys::{
        base::{Boolean, CFRelease, CFTypeRef},
        number::kCFBooleanFalse,
        preferences::{
            CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, CFPreferencesSetAppValue,
            kCFPreferencesCurrentApplication,
        },
        string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8},
    };

    struct PreferenceKey {
        name: &'static str,
        source: &'static str,
    }

    pub fn apply() {
        let keys = [
            PreferenceKey {
                name: "WebKitWritingToolsEnabled",
                source: "Tyde requirement: documented WebKit preference key used to disable macOS 15+ Writing Tools overlays.",
            },
            // Source: WebKit TextCheckerMac.mm defines WebAutomaticTextReplacementEnabled and
            // reads it from NSUserDefaults for WebKit text-checking state:
            // https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm
            PreferenceKey {
                name: "WebAutomaticTextReplacementEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
            // Source: same WebKit TextCheckerMac.mm file; this key controls automatic spelling
            // correction state for WebKit text input.
            PreferenceKey {
                name: "WebAutomaticSpellingCorrectionEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
            // Source: Chromium's macOS Cocoa bridge uses this NSUserDefaults key for WebKit quote
            // substitution and toggles it directly via NSUserDefaults:
            // https://chromium.googlesource.com/chromium/src/+/a07e14909ad9a17cc721f42b67a6e5aa56d27bc7/content/app_shim_remote_cocoa/render_widget_host_view_cocoa.mm
            PreferenceKey {
                name: "WebAutomaticQuoteSubstitutionEnabled",
                source: "https://chromium.googlesource.com/chromium/src/+/a07e14909ad9a17cc721f42b67a6e5aa56d27bc7/content/app_shim_remote_cocoa/render_widget_host_view_cocoa.mm",
            },
            // Source: same Chromium macOS Cocoa bridge file; this key controls dash substitution
            // in WebKit-backed text input.
            PreferenceKey {
                name: "WebAutomaticDashSubstitutionEnabled",
                source: "https://chromium.googlesource.com/chromium/src/+/a07e14909ad9a17cc721f42b67a6e5aa56d27bc7/content/app_shim_remote_cocoa/render_widget_host_view_cocoa.mm",
            },
            // Source: WebKit TextCheckerMac.mm defines this key and initializes WebKit's
            // continuous spell-checking state from NSUserDefaults.
            PreferenceKey {
                name: "WebContinuousSpellCheckingEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
            // Source: same WebKit TextCheckerMac.mm file; this key controls grammar checking.
            PreferenceKey {
                name: "WebGrammarCheckingEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
            // Source: same WebKit TextCheckerMac.mm file; this key controls smart insert/delete.
            PreferenceKey {
                name: "WebSmartInsertDeleteEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
            // Source: same WebKit TextCheckerMac.mm file; WebKit uses this defaults key for
            // automatic link detection (closest verified text-checking/data-detector toggle).
            PreferenceKey {
                name: "WebAutomaticLinkDetectionEnabled",
                source: "https://chromium.googlesource.com/external/WebKit_submodule/+/eb8d4fdea1324f303f36d281d89b8341d13824d3/Source/WebKit2/UIProcess/mac/TextCheckerMac.mm",
            },
        ];

        for key in keys {
            if let Err(err) = set_current_app_boolean_false(key.name) {
                eprintln!(
                    "warning: failed to set macOS WebKit default {}=false (source: {}): {}",
                    key.name, key.source, err
                );
            }
        }
    }

    fn set_current_app_boolean_false(key: &str) -> Result<(), String> {
        let key_ref = create_cf_string(key)?;
        let value_ref = unsafe { kCFBooleanFalse as CFTypeRef };

        unsafe {
            CFPreferencesSetAppValue(key_ref, value_ref, kCFPreferencesCurrentApplication);
        }

        let synchronized = unsafe { CFPreferencesAppSynchronize(kCFPreferencesCurrentApplication) };
        if synchronized == 0 as Boolean {
            unsafe {
                CFRelease(key_ref as CFTypeRef);
            }
            return Err("CFPreferencesAppSynchronize returned false".to_owned());
        }

        let stored_value =
            unsafe { CFPreferencesCopyAppValue(key_ref, kCFPreferencesCurrentApplication) };
        let applied = stored_value == value_ref;

        unsafe {
            if !stored_value.is_null() {
                CFRelease(stored_value);
            }
            CFRelease(key_ref as CFTypeRef);
        }

        if !applied {
            return Err("readback did not match kCFBooleanFalse".to_owned());
        }

        Ok(())
    }

    fn create_cf_string(value: &str) -> Result<CFStringRef, String> {
        let c_string =
            CString::new(value).map_err(|err| format!("invalid preference key string: {err}"))?;
        let string_ref = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c_string.as_ptr(), kCFStringEncodingUTF8)
        };
        if string_ref.is_null() {
            return Err("CFStringCreateWithCString returned null".to_owned());
        }
        Ok(string_ref)
    }
}

struct ShellState {
    router: ProxyRouterHandle,
    host: server::HostHandle,
    host_store: HostStore,
    ui_debug: Arc<devtools::UiDebugBridgeState>,
}

#[tauri::command]
async fn connect_host(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<(), String> {
    let configured_host = state
        .host_store
        .get(&host_id)?
        .ok_or_else(|| format!("configured host '{}' not found", host_id))?;
    state
        .router
        .connect_local(app, host_id, configured_host.transport, state.host.clone())
        .await
}

#[tauri::command]
async fn disconnect_host(
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<(), String> {
    state.router.disconnect(host_id).await
}

#[tauri::command]
async fn send_host_line(
    state: tauri::State<'_, ShellState>,
    host_id: String,
    line: String,
) -> Result<(), String> {
    state.router.send_line(host_id, line).await
}

#[tauri::command]
fn list_configured_hosts(
    state: tauri::State<'_, ShellState>,
) -> Result<ConfiguredHostStore, String> {
    state.host_store.list()
}

#[tauri::command]
fn upsert_configured_host(
    state: tauri::State<'_, ShellState>,
    request: UpsertConfiguredHostRequest,
) -> Result<ConfiguredHostStore, String> {
    state.host_store.upsert(request)
}

#[tauri::command]
async fn remove_configured_host(
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<ConfiguredHostStore, String> {
    let _ = state.router.disconnect(host_id.clone()).await;
    state.host_store.remove(&host_id)
}

#[tauri::command]
fn set_selected_host(
    state: tauri::State<'_, ShellState>,
    host_id: Option<String>,
) -> Result<ConfiguredHostStore, String> {
    state.host_store.set_selected_host(host_id)
}

#[tauri::command]
fn mark_ui_debug_ready(state: tauri::State<'_, ShellState>) {
    state.ui_debug.mark_ready();
}

#[tauri::command]
async fn submit_ui_debug_response(
    state: tauri::State<'_, ShellState>,
    request_id: String,
    response: devtools_protocol::UiDebugResponse,
) -> Result<(), String> {
    state
        .ui_debug
        .submit_response(UiDebugResponseSubmission {
            request_id,
            response,
        })
        .await
}

#[tauri::command]
async fn submit_feedback(feedback: String) -> Result<(), String> {
    let client = reqwest::Client::new();
    let params = [("entry.515008519", feedback.as_str())];
    client
        .post("https://docs.google.com/forms/d/e/1FAIpQLSfcaoYqtm0FRdibE5qJhVYONUbKAMn6KTIopx40Fk8l9yn2vA/formResponse")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to send feedback: {e}"))?;
    Ok(())
}

pub fn run() {
    #[cfg(target_os = "macos")]
    macos_webview_defaults::apply();

    if let Err(err) = logging::init_gui_logging() {
        eprintln!("warning: failed to initialize GUI logging: {err}");
    }

    tracing::info!("starting tyde shell");

    tauri::Builder::default()
        .setup(|app| {
            tracing::info!("setup: spawning host and router");
            let host_store_path =
                host_store::HostStore::default_path().map_err(std::io::Error::other)?;
            let host_store =
                host_store::HostStore::load(host_store_path).map_err(std::io::Error::other)?;
            let router = ProxyRouterHandle::new();
            let ui_debug = Arc::new(devtools::UiDebugBridgeState::default());
            let ui_debug_addr =
                devtools::start_ui_debug_http_server(app.handle(), ui_debug.clone())
                    .map_err(std::io::Error::other)?;
            if let Some(url) = &ui_debug_addr {
                tracing::info!("ui debug HTTP server ready at {url}");
            }

            let host = server::spawn_host_with_store_paths_and_runtime_config(
                server::store::session::SessionStore::default_path()
                    .map_err(std::io::Error::other)?,
                server::store::project::ProjectStore::default_path()
                    .map_err(std::io::Error::other)?,
                server::store::settings::HostSettingsStore::default_path()
                    .map_err(std::io::Error::other)?,
                server::HostRuntimeConfig::default(),
            )
            .map_err(std::io::Error::other)?;

            if let Some(addr) =
                dev_host::start_dev_host_listener(host.clone()).map_err(std::io::Error::other)?
            {
                tracing::info!("dev host listener ready at {addr}");
            }

            app.manage(ShellState {
                router,
                host,
                host_store,
                ui_debug,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line,
            list_configured_hosts,
            upsert_configured_host,
            remove_configured_host,
            set_selected_host,
            mark_ui_debug_ready,
            submit_ui_debug_response,
            submit_feedback
        ])
        .run(tauri::generate_context!())
        .expect("failed to run desktop shell");
}

pub fn run_host_stdio() -> Result<(), String> {
    host_stdio::run()
}

pub fn run_host_uds() -> Result<(), String> {
    host_uds::run()
}

pub fn run_host_bridge_uds() -> Result<(), String> {
    host_bridge_uds::run()
}
