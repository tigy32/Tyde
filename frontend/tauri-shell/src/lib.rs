mod bridge;
mod dev_host;
mod devtools;
mod host_bridge_uds;
mod host_stdio;
mod host_store;
mod host_uds;
mod logging;
mod remote_bootstrap;
mod router;

use std::{
    collections::{HashMap, VecDeque},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use devtools_protocol::UiDebugResponseSubmission;
use host_config::RemoteHostLifecycleSnapshot;
use host_store::{ConfiguredHostStore, HostStore, UpsertConfiguredHostRequest};
use router::ProxyRouterHandle;
use tauri::{AppHandle, Manager, RunEvent, Url, WindowEvent, webview::PageLoadEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

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
    web_content_recovery: Arc<Mutex<WebContentRecoveryPolicies>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryDecision {
    Reload {
        generation: u64,
    },
    Suppress {
        generation: u64,
    },
    Escalate {
        generation: u64,
        failure: RecoveryFailure,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryFailure {
    ReloadFailed,
    ReadinessDeadline,
    AttemptLimit,
}

const RECOVERY_ATTEMPT_LIMIT: usize = 3;
const RECOVERY_ATTEMPT_WINDOW: Duration = Duration::from_secs(5 * 60);
const RECOVERY_READINESS_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Default)]
struct WebContentRecoveryPolicy {
    generation: u64,
    reload_pending: bool,
    page_load_finished: bool,
    frontend_ready: bool,
    failure_presented: bool,
    attempts: VecDeque<Instant>,
}

impl WebContentRecoveryPolicy {
    fn web_content_process_terminated(&mut self, now: Instant) -> RecoveryDecision {
        if self.reload_pending || self.failure_presented {
            return RecoveryDecision::Suppress {
                generation: self.generation,
            };
        }
        self.attempts.retain(|attempt| {
            now.checked_duration_since(*attempt)
                .is_some_and(|age| age <= RECOVERY_ATTEMPT_WINDOW)
        });
        if self.attempts.len() >= RECOVERY_ATTEMPT_LIMIT {
            self.failure_presented = true;
            return RecoveryDecision::Escalate {
                generation: self.generation,
                failure: RecoveryFailure::AttemptLimit,
            };
        }

        self.generation = self.generation.wrapping_add(1);
        self.attempts.push_back(now);
        self.reload_pending = true;
        self.page_load_finished = false;
        self.frontend_ready = false;
        RecoveryDecision::Reload {
            generation: self.generation,
        }
    }

    fn page_load_started(&mut self) {
        if self.reload_pending {
            self.page_load_finished = false;
            self.frontend_ready = false;
        }
    }

    fn page_load_finished(&mut self) {
        self.page_load_finished = true;
        self.rearm_if_ready();
    }

    fn frontend_ready(&mut self) {
        self.frontend_ready = true;
        self.rearm_if_ready();
    }

    fn rearm_if_ready(&mut self) {
        if self.page_load_finished && self.frontend_ready {
            self.reload_pending = false;
        }
    }

    fn reload_failed(&mut self, generation: u64) -> Option<RecoveryFailure> {
        self.fail_if_pending(generation, RecoveryFailure::ReloadFailed)
    }

    fn readiness_deadline_elapsed(&mut self, generation: u64) -> Option<RecoveryFailure> {
        self.fail_if_pending(generation, RecoveryFailure::ReadinessDeadline)
    }

    fn fail_if_pending(
        &mut self,
        generation: u64,
        failure: RecoveryFailure,
    ) -> Option<RecoveryFailure> {
        if self.generation != generation || !self.reload_pending || self.failure_presented {
            return None;
        }
        self.failure_presented = true;
        Some(failure)
    }
}

#[derive(Default)]
struct WebContentRecoveryPolicies {
    by_label: HashMap<String, WebContentRecoveryPolicy>,
}

impl WebContentRecoveryPolicies {
    fn for_label(&mut self, label: &str) -> &mut WebContentRecoveryPolicy {
        self.by_label.entry(label.to_owned()).or_default()
    }
}

fn with_recovery_policies<T>(
    recovery: &Mutex<WebContentRecoveryPolicies>,
    f: impl FnOnce(&mut WebContentRecoveryPolicies) -> T,
) -> T {
    let mut policies = recovery.lock().unwrap_or_else(|poisoned| {
        tracing::error!("webview.recovery event=policy_lock_poisoned");
        poisoned.into_inner()
    });
    f(&mut policies)
}

fn shutdown_managed_host(app: &AppHandle) {
    let host = app.state::<ShellState>().host.clone();
    tauri::async_runtime::block_on(host.shutdown_spawn_operations());
}

#[derive(Default)]
struct QuitConfirmation {
    confirmed_exit: AtomicBool,
    dialog_open: AtomicBool,
}

impl QuitConfirmation {
    fn is_confirmed_exit(&self) -> bool {
        self.confirmed_exit.load(Ordering::SeqCst)
    }

    fn consume_confirmed_exit(&self) -> bool {
        self.confirmed_exit.swap(false, Ordering::SeqCst)
    }

    fn mark_confirmed_exit(&self) {
        self.confirmed_exit.store(true, Ordering::SeqCst);
    }

    fn try_open_dialog(&self) -> bool {
        !self.dialog_open.swap(true, Ordering::SeqCst)
    }

    fn close_dialog(&self) {
        self.dialog_open.store(false, Ordering::SeqCst);
    }
}

fn request_quit_confirmation(app: tauri::AppHandle, confirmation: Arc<QuitConfirmation>) {
    if !confirmation.try_open_dialog() {
        return;
    }

    let mut dialog = app
        .dialog()
        .message("Are you sure you want to quit Tyde?")
        .title("Quit Tyde?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Quit".to_owned(),
            "Cancel".to_owned(),
        ));

    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.parent(&window);
    }

    dialog.show(move |should_quit| {
        confirmation.close_dialog();
        if should_quit {
            confirmation.mark_confirmed_exit();
            app.exit(0);
        }
    });
}

fn show_web_content_recovery_failure(
    app: tauri::AppHandle,
    label: String,
    confirmation: Arc<QuitConfirmation>,
    generation: u64,
    failure: RecoveryFailure,
) {
    tracing::error!(
        "webview.recovery event=recovery_failed label={label} generation={generation} failure={failure:?} action=prompt_restart_or_quit"
    );
    let message = match failure {
        RecoveryFailure::ReloadFailed => {
            "Tyde could not reload its interface after the web content process stopped."
        }
        RecoveryFailure::ReadinessDeadline => {
            "Tyde reloaded its interface, but it did not become ready in time."
        }
        RecoveryFailure::AttemptLimit => {
            "Tyde's interface stopped repeatedly. Automatic recovery has been disabled."
        }
    };
    let mut dialog = app
        .dialog()
        .message(format!(
            "{message}\n\nRestart Tyde to reconnect safely, or quit without restarting."
        ))
        .title("Tyde recovery failed")
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Restart".to_owned(),
            "Quit".to_owned(),
        ));
    if let Some(window) = app.get_webview_window(&label) {
        dialog = dialog.parent(&window);
    }
    dialog.show(move |restart| {
        if restart {
            app.request_restart();
        } else {
            confirmation.mark_confirmed_exit();
            app.exit(1);
        }
    });
}

fn schedule_recovery_readiness_deadline(
    app: tauri::AppHandle,
    label: String,
    generation: u64,
    recovery: Arc<Mutex<WebContentRecoveryPolicies>>,
    confirmation: Arc<QuitConfirmation>,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(RECOVERY_READINESS_DEADLINE).await;
        let failure = with_recovery_policies(&recovery, |policies| {
            policies
                .for_label(&label)
                .readiness_deadline_elapsed(generation)
        });
        if let Some(failure) = failure {
            let app_for_dialog = app.clone();
            if let Err(error) = app.run_on_main_thread(move || {
                show_web_content_recovery_failure(
                    app_for_dialog,
                    label,
                    confirmation,
                    generation,
                    failure,
                );
            }) {
                tracing::error!(
                    "webview.recovery event=failure_dialog_dispatch_failed generation={generation} error={error}"
                );
            }
        }
    });
}

fn external_link_guard<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("external-link-guard")
        .on_navigation(|webview, url| {
            let dev_url = dev_server_url(webview.config());
            if !should_open_externally(url, dev_url) {
                return true;
            }

            if let Err(err) = open_url_with_system_handler(url, dev_url) {
                tracing::warn!("failed to open external navigation {url}: {err}");
            }
            false
        })
        .build()
}

/// Dev instances launched by the debug MCP rewrite `build.devUrl` to a random
/// loopback port, so the guard reads the configured dev URL rather than
/// assuming 1420. Release builds still carry a `devUrl`, hence the profile gate.
fn dev_server_url(config: &tauri::Config) -> Option<&Url> {
    if !cfg!(debug_assertions) {
        return None;
    }

    config.build.dev_url.as_ref()
}

fn should_open_externally(url: &Url, dev_url: Option<&Url>) -> bool {
    if is_app_url(url, dev_url) {
        return false;
    }

    matches!(url.scheme(), "http" | "https" | "mailto")
}

fn is_app_url(url: &Url, dev_url: Option<&Url>) -> bool {
    match url.scheme() {
        "tauri" | "asset" | "ipc" => return true,
        "http" | "https" => {}
        _ => return false,
    }

    if matches!(
        url.host_str(),
        Some("tauri.localhost") | Some("asset.localhost")
    ) {
        return true;
    }

    dev_url.is_some_and(|dev_url| is_dev_server_origin(url, dev_url))
}

fn is_dev_server_origin(url: &Url, dev_url: &Url) -> bool {
    url.scheme() == dev_url.scheme()
        && url.port_or_known_default() == dev_url.port_or_known_default()
        && match (url.host_str(), dev_url.host_str()) {
            (Some(host), Some(dev_host)) => {
                host == dev_host || (is_loopback_host(host) && is_loopback_host(dev_host))
            }
            _ => false,
        }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn parse_external_url(value: &str, dev_url: Option<&Url>) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|err| format!("invalid URL: {err}"))?;
    if is_app_url(&url, dev_url) {
        return Err("refusing to open Tyde's own app URL externally".to_owned());
    }

    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Ok(url),
        "http" | "https" => Err("URL must include a host".to_owned()),
        "mailto" if !url.path().is_empty() => Ok(url),
        "mailto" => Err("mailto URL must include an address".to_owned()),
        scheme => Err(format!("unsupported external URL scheme: {scheme}")),
    }
}

fn open_url_with_system_handler(url: &Url, dev_url: Option<&Url>) -> Result<(), String> {
    let url = parse_external_url(url.as_str(), dev_url)?;
    spawn_system_url_handler(url.as_str())
}

fn spawn_system_url_handler(url: &str) -> Result<(), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = url;
        return Err("opening external links is not supported on this platform".to_owned());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let mut command = system_url_handler_command(url);
        command
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("failed to launch system URL handler: {err}"))
    }
}

#[cfg(target_os = "macos")]
fn system_url_handler_command(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(target_os = "windows")]
fn system_url_handler_command(url: &str) -> Command {
    let mut command = Command::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler").arg(url);
    command
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(any(target_os = "android", target_os = "ios"))
))]
fn system_url_handler_command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

#[tauri::command]
fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let url = parse_external_url(&url, dev_server_url(app.config()))?;
    spawn_system_url_handler(url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_url(port: u16) -> Url {
        Url::parse(&format!("http://127.0.0.1:{port}")).expect("dev url should parse")
    }

    #[test]
    fn external_url_validation_allows_web_and_mail_links() {
        let dev = dev_url(1420);
        let dev = Some(&dev);
        assert!(parse_external_url("https://example.com/path?q=1", dev).is_ok());
        assert!(parse_external_url("http://example.com", dev).is_ok());
        assert!(parse_external_url("mailto:help@example.com", dev).is_ok());
    }

    #[test]
    fn external_url_validation_rejects_unsafe_or_internal_targets() {
        let dev = dev_url(1420);
        let dev = Some(&dev);
        assert!(parse_external_url("javascript:alert(1)", dev).is_err());
        assert!(parse_external_url("file:///etc/passwd", dev).is_err());
        assert!(parse_external_url("https://", dev).is_err());
        assert!(parse_external_url("tauri://localhost", dev).is_err());
        assert!(parse_external_url("http://tauri.localhost/", dev).is_err());
        assert!(parse_external_url("http://127.0.0.1:1420/", dev).is_err());
    }

    #[test]
    fn navigation_guard_opens_only_external_urls() {
        let dev = dev_url(1420);
        let dev = Some(&dev);
        assert!(!should_open_externally(
            &Url::parse("tauri://localhost").unwrap(),
            dev
        ));
        assert!(!should_open_externally(
            &Url::parse("http://tauri.localhost/").unwrap(),
            dev
        ));
        assert!(should_open_externally(
            &Url::parse("https://example.com").unwrap(),
            dev
        ));
    }

    #[test]
    fn navigation_guard_keeps_configured_dev_server_in_the_webview() {
        let dev = dev_url(51763);
        let dev = Some(&dev);
        assert!(!should_open_externally(
            &Url::parse("http://127.0.0.1:51763/").unwrap(),
            dev
        ));
        assert!(!should_open_externally(
            &Url::parse("http://localhost:51763/index.html#/agents").unwrap(),
            dev
        ));
    }

    #[test]
    fn navigation_guard_does_not_whitelist_other_origins() {
        let dev = dev_url(51763);
        let dev = Some(&dev);
        assert!(should_open_externally(
            &Url::parse("http://127.0.0.1:1420/").unwrap(),
            dev
        ));
        assert!(should_open_externally(
            &Url::parse("http://127.0.0.1:51764/").unwrap(),
            dev
        ));
        assert!(should_open_externally(
            &Url::parse("http://evil.example.com:51763/").unwrap(),
            dev
        ));
        assert!(should_open_externally(
            &Url::parse("https://127.0.0.1:51763/").unwrap(),
            dev
        ));
    }

    #[test]
    fn navigation_guard_without_dev_server_treats_loopback_as_external() {
        assert!(should_open_externally(
            &Url::parse("http://127.0.0.1:1420/").unwrap(),
            None
        ));
        assert!(!should_open_externally(
            &Url::parse("http://tauri.localhost/").unwrap(),
            None
        ));
    }
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
async fn probe_configured_host_lifecycle(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let configured_host = state
        .host_store
        .get(&host_id)?
        .ok_or_else(|| format!("configured host '{}' not found", host_id))?;
    remote_bootstrap::probe_configured_host_lifecycle(app, configured_host).await
}

#[tauri::command]
async fn ensure_configured_host_ready(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let configured_host = state
        .host_store
        .get(&host_id)?
        .ok_or_else(|| format!("configured host '{}' not found", host_id))?;
    remote_bootstrap::ensure_configured_host_ready(app, configured_host).await
}

#[tauri::command]
async fn force_upgrade_managed_host(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let configured_host = state
        .host_store
        .get(&host_id)?
        .ok_or_else(|| format!("configured host '{}' not found", host_id))?;
    remote_bootstrap::force_upgrade_managed_host(app, configured_host).await
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
fn mark_frontend_ready(webview: tauri::Webview, state: tauri::State<'_, ShellState>) {
    let label = webview.label();
    let generation = with_recovery_policies(&state.web_content_recovery, |policies| {
        let policy = policies.for_label(label);
        policy.frontend_ready();
        policy.generation
    });
    tracing::info!("webview.lifecycle event=frontend_ready label={label} generation={generation}");
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrontendLifecycleEvent {
    Visible,
    Hidden,
    Focus,
    Blur,
    PageShow,
    PageHide,
}

impl FrontendLifecycleEvent {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Focus => "focus",
            Self::Blur => "blur",
            Self::PageShow => "page_show",
            Self::PageHide => "page_hide",
        }
    }
}

#[tauri::command]
fn report_frontend_lifecycle(event: FrontendLifecycleEvent) {
    tracing::info!("webview.lifecycle event={}", event.as_str());
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

    let quit_confirmation = Arc::new(QuitConfirmation::default());
    let quit_confirmation_for_window = quit_confirmation.clone();
    let quit_confirmation_for_run = quit_confirmation.clone();
    let web_content_recovery = Arc::new(Mutex::new(WebContentRecoveryPolicies::default()));
    let recovery_for_page_load = web_content_recovery.clone();
    let recovery_for_setup = web_content_recovery.clone();

    let builder = tauri::Builder::default()
        .plugin(external_link_guard())
        .plugin(tauri_plugin_dialog::init())
        .on_page_load(move |webview, payload| {
            let label = webview.label();
            let generation = with_recovery_policies(&recovery_for_page_load, |policies| {
                let policy = policies.for_label(label);
                match payload.event() {
                    PageLoadEvent::Started => policy.page_load_started(),
                    PageLoadEvent::Finished => policy.page_load_finished(),
                }
                policy.generation
            });
            let event = match payload.event() {
                PageLoadEvent::Started => "page_load_started",
                PageLoadEvent::Finished => "page_load_finished",
            };
            tracing::info!(
                "webview.lifecycle event={event} label={} generation={generation}",
                webview.label()
            );
        })
        .on_window_event(move |window, event| match event {
            WindowEvent::Focused(focused) => {
                tracing::info!(
                    "webview.lifecycle event=window_focus label={} focused={focused}",
                    window.label()
                );
            }
            WindowEvent::CloseRequested { api, .. } => {
                if quit_confirmation_for_window.is_confirmed_exit() {
                    return;
                }

                api.prevent_close();
                request_quit_confirmation(
                    window.app_handle().clone(),
                    quit_confirmation_for_window.clone(),
                );
            }
            _ => {}
        });

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let builder = {
        let recovery_for_termination = web_content_recovery.clone();
        let confirmation_for_recovery = quit_confirmation.clone();
        builder.on_web_content_process_terminate(move |webview| {
            let label = webview.label().to_owned();
            let decision = with_recovery_policies(&recovery_for_termination, |policies| {
                policies
                    .for_label(&label)
                    .web_content_process_terminated(Instant::now())
            });
            match decision {
                RecoveryDecision::Reload { generation } => {
                    tracing::warn!(
                        "webview.recovery event=web_content_process_terminated label={} generation={generation} action=reload",
                        webview.label()
                    );
                    match webview.reload() {
                        Ok(()) => schedule_recovery_readiness_deadline(
                            webview.app_handle().clone(),
                            label,
                            generation,
                            recovery_for_termination.clone(),
                            confirmation_for_recovery.clone(),
                        ),
                        Err(error) => {
                            tracing::error!(
                                "webview.recovery event=reload_failed label={} generation={generation} error={error}",
                                webview.label()
                            );
                            let failure =
                                with_recovery_policies(&recovery_for_termination, |policies| {
                                    policies.for_label(&label).reload_failed(generation)
                                });
                            if let Some(failure) = failure {
                                show_web_content_recovery_failure(
                                    webview.app_handle().clone(),
                                    label,
                                    confirmation_for_recovery.clone(),
                                    generation,
                                    failure,
                                );
                            }
                        }
                    }
                }
                RecoveryDecision::Suppress { generation } => {
                    tracing::error!(
                        "webview.recovery event=web_content_process_terminated label={} generation={generation} action=suppress_pending",
                        webview.label()
                    );
                }
                RecoveryDecision::Escalate {
                    generation,
                    failure,
                } => {
                    show_web_content_recovery_failure(
                        webview.app_handle().clone(),
                        label,
                        confirmation_for_recovery.clone(),
                        generation,
                        failure,
                    );
                }
            }
        })
    };

    let app = builder
        .setup(move |app| {
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
                web_content_recovery: recovery_for_setup,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line,
            probe_configured_host_lifecycle,
            ensure_configured_host_ready,
            force_upgrade_managed_host,
            list_configured_hosts,
            upsert_configured_host,
            remove_configured_host,
            set_selected_host,
            mark_ui_debug_ready,
            mark_frontend_ready,
            report_frontend_lifecycle,
            submit_ui_debug_response,
            submit_feedback,
            open_external_url
        ])
        .build(tauri::generate_context!())
        .expect("failed to build desktop shell");

    app.run(move |app, event| {
        if let RunEvent::ExitRequested { code, api, .. } = event {
            if code == Some(tauri::RESTART_EXIT_CODE)
                || quit_confirmation_for_run.consume_confirmed_exit()
            {
                shutdown_managed_host(app);
                return;
            }

            api.prevent_exit();
            request_quit_confirmation(app.clone(), quit_confirmation_for_run.clone());
        }
    });
}

pub fn run_host_stdio() -> Result<(), String> {
    host_stdio::run()
}

pub fn run_host_uds() -> Result<(), String> {
    host_uds::run()
}

pub fn run_host_status_uds() -> Result<(), String> {
    host_uds::status()
}

pub fn run_host_launch_uds() -> Result<(), String> {
    host_uds::launch()
}

pub fn run_host_bridge_uds() -> Result<(), String> {
    host_bridge_uds::run()
}

#[cfg(test)]
mod web_content_recovery_tests {
    use std::time::{Duration, Instant};

    use super::{
        RECOVERY_ATTEMPT_LIMIT, RECOVERY_ATTEMPT_WINDOW, RecoveryDecision, RecoveryFailure,
        WebContentRecoveryPolicies, WebContentRecoveryPolicy,
    };

    #[test]
    fn termination_reloads_once_until_the_new_page_is_ready() {
        let mut policy = WebContentRecoveryPolicy::default();
        let now = Instant::now();

        assert_eq!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 1 }
        );
        assert_eq!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Suppress { generation: 1 },
            "a repeated termination before recovery completes must not reload-loop"
        );

        policy.page_load_started();
        policy.page_load_finished();
        assert_eq!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Suppress { generation: 1 },
            "load completion without the frontend-ready acknowledgement is not recovery"
        );

        policy.frontend_ready();
        assert_eq!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 2 },
            "a later independent process death is recoverable after the page is healthy"
        );
    }

    #[test]
    fn ready_and_load_finished_rearm_in_either_order() {
        let mut policy = WebContentRecoveryPolicy::default();
        let now = Instant::now();
        assert!(matches!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 1 }
        ));

        policy.frontend_ready();
        assert!(matches!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Suppress { generation: 1 }
        ));

        policy.page_load_finished();
        assert!(matches!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 2 }
        ));
    }

    #[test]
    fn deadline_and_reload_error_escalate_only_a_termination_recovery() {
        let now = Instant::now();
        let mut deadline = WebContentRecoveryPolicy::default();
        assert_eq!(
            deadline.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 1 }
        );
        assert_eq!(
            deadline.readiness_deadline_elapsed(1),
            Some(RecoveryFailure::ReadinessDeadline)
        );
        assert_eq!(deadline.readiness_deadline_elapsed(1), None);

        let mut reload_error = WebContentRecoveryPolicy::default();
        assert_eq!(
            reload_error.web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 1 }
        );
        assert_eq!(
            reload_error.reload_failed(1),
            Some(RecoveryFailure::ReloadFailed)
        );

        let mut normal_page = WebContentRecoveryPolicy::default();
        assert_eq!(
            normal_page.readiness_deadline_elapsed(0),
            None,
            "normal startup and unrelated frontend panics have no recovery deadline"
        );
    }

    #[test]
    fn rolling_attempt_limit_escalates_then_expires() {
        let now = Instant::now();
        let mut policy = WebContentRecoveryPolicy::default();
        for generation in 1..=RECOVERY_ATTEMPT_LIMIT {
            assert_eq!(
                policy.web_content_process_terminated(now),
                RecoveryDecision::Reload {
                    generation: generation as u64
                }
            );
            policy.page_load_finished();
            policy.frontend_ready();
        }
        assert_eq!(
            policy.web_content_process_terminated(now),
            RecoveryDecision::Escalate {
                generation: RECOVERY_ATTEMPT_LIMIT as u64,
                failure: RecoveryFailure::AttemptLimit,
            }
        );

        let mut expired = WebContentRecoveryPolicy::default();
        for _ in 0..RECOVERY_ATTEMPT_LIMIT {
            assert!(matches!(
                expired.web_content_process_terminated(now),
                RecoveryDecision::Reload { .. }
            ));
            expired.page_load_finished();
            expired.frontend_ready();
        }
        assert!(matches!(
            expired.web_content_process_terminated(
                now + RECOVERY_ATTEMPT_WINDOW + Duration::from_secs(1)
            ),
            RecoveryDecision::Reload { .. }
        ));
    }

    #[test]
    fn recovery_state_is_scoped_by_webview_label() {
        let now = Instant::now();
        let mut policies = WebContentRecoveryPolicies::default();
        assert!(matches!(
            policies
                .for_label("main")
                .web_content_process_terminated(now),
            RecoveryDecision::Reload { generation: 1 }
        ));
        policies.for_label("auth").page_load_finished();
        policies.for_label("auth").frontend_ready();
        assert!(matches!(
            policies
                .for_label("main")
                .web_content_process_terminated(now),
            RecoveryDecision::Suppress { generation: 1 }
        ));
        assert_eq!(policies.for_label("auth").generation, 0);
    }
}
