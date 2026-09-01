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
#[cfg(not(target_os = "windows"))]
mod voice_media;

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
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};

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
    #[cfg(not(target_os = "windows"))]
    voice_media: voice_media::NativeVoiceMedia,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryDecision {
    Reload {
        generation: u64,
    },
    Suppress {
        generation: u64,
        terminated_while_pending: bool,
    },
    Escalate {
        generation: u64,
        failure: RecoveryFailure,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryFailure {
    ReloadFailed,
    RepeatedTermination,
    ReadinessDeadline,
    AttemptLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoveryDeadlineTicket {
    generation: u64,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryDialogAction {
    KeepWaiting,
    Restart,
}

const RECOVERY_RESTART_LABEL: &str = "Restart";
const RECOVERY_KEEP_WAITING_LABEL: &str = "Keep Waiting";
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
    readiness_notice_presented: bool,
    terminated_while_pending: bool,
    frontend_visible: Option<bool>,
    native_window_focused: Option<bool>,
    readiness_deadline_epoch: u64,
    attempts: VecDeque<Instant>,
}

impl WebContentRecoveryPolicy {
    fn web_content_process_terminated(&mut self, now: Instant) -> RecoveryDecision {
        if self.failure_presented {
            return RecoveryDecision::Suppress {
                generation: self.generation,
                terminated_while_pending: false,
            };
        }
        if self.reload_pending {
            self.terminated_while_pending = true;
            self.frontend_visible = None;
            if let Some(failure) = self.escalate_repeated_termination_if_observable() {
                return RecoveryDecision::Escalate {
                    generation: self.generation,
                    failure,
                };
            }
            return RecoveryDecision::Suppress {
                generation: self.generation,
                terminated_while_pending: true,
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
        self.readiness_notice_presented = false;
        self.terminated_while_pending = false;
        self.frontend_visible = None;
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        RecoveryDecision::Reload {
            generation: self.generation,
        }
    }

    fn page_load_started(&mut self) {
        if self.reload_pending {
            self.frontend_visible = None;
            if self.terminated_while_pending {
                self.terminated_while_pending = false;
                self.failure_presented = false;
                self.readiness_notice_presented = false;
                self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
            }
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
        if self.page_load_finished && self.frontend_ready && !self.terminated_while_pending {
            self.reload_pending = false;
            self.readiness_notice_presented = false;
            self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        }
    }

    fn reload_failed(&mut self, generation: u64) -> Option<RecoveryFailure> {
        self.fail_if_pending(generation, RecoveryFailure::ReloadFailed)
    }

    fn start_readiness_deadline(&mut self, generation: u64) -> Option<RecoveryDeadlineTicket> {
        if self.generation != generation
            || !self.reload_pending
            || self.failure_presented
            || self.readiness_notice_presented
            || self.readiness_is_deferred()
        {
            return None;
        }
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        Some(RecoveryDeadlineTicket {
            generation,
            epoch: self.readiness_deadline_epoch,
        })
    }

    fn frontend_visibility_changed(&mut self, visible: bool) -> Option<RecoveryDeadlineTicket> {
        if self.frontend_visible == Some(visible) {
            return None;
        }
        self.frontend_visible = Some(visible);
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        if visible {
            self.start_readiness_deadline(self.generation)
        } else {
            None
        }
    }

    fn native_window_focus_changed(&mut self, focused: bool) -> Option<RecoveryDeadlineTicket> {
        if self.native_window_focused == Some(focused) {
            return None;
        }
        self.native_window_focused = Some(focused);
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        if focused {
            self.start_readiness_deadline(self.generation)
        } else {
            None
        }
    }

    fn readiness_is_deferred(&self) -> bool {
        self.frontend_visible == Some(false) || self.native_window_focused == Some(false)
    }

    fn escalate_repeated_termination_if_observable(&mut self) -> Option<RecoveryFailure> {
        if !self.reload_pending
            || !self.terminated_while_pending
            || self.failure_presented
            || self.readiness_is_deferred()
        {
            return None;
        }
        self.failure_presented = true;
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
        Some(RecoveryFailure::RepeatedTermination)
    }

    fn readiness_deadline_elapsed(
        &mut self,
        ticket: RecoveryDeadlineTicket,
    ) -> Option<RecoveryFailure> {
        if self.generation != ticket.generation
            || self.readiness_deadline_epoch != ticket.epoch
            || !self.reload_pending
            || self.failure_presented
            || self.readiness_notice_presented
            || self.readiness_is_deferred()
        {
            return None;
        }
        if self.terminated_while_pending {
            self.escalate_repeated_termination_if_observable()
        } else {
            self.readiness_notice_presented = true;
            Some(RecoveryFailure::ReadinessDeadline)
        }
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
        self.readiness_deadline_epoch = self.readiness_deadline_epoch.wrapping_add(1);
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

fn recovery_dialog_buttons(failure: RecoveryFailure) -> MessageDialogButtons {
    match failure {
        RecoveryFailure::ReadinessDeadline => {
            MessageDialogButtons::OkCustom(RECOVERY_KEEP_WAITING_LABEL.to_owned())
        }
        RecoveryFailure::ReloadFailed
        | RecoveryFailure::RepeatedTermination
        | RecoveryFailure::AttemptLimit => MessageDialogButtons::OkCancelCustom(
            RECOVERY_RESTART_LABEL.to_owned(),
            RECOVERY_KEEP_WAITING_LABEL.to_owned(),
        ),
    }
}

fn recovery_dialog_action(
    failure: RecoveryFailure,
    result: MessageDialogResult,
) -> RecoveryDialogAction {
    if !matches!(failure, RecoveryFailure::ReadinessDeadline)
        && result == MessageDialogResult::Custom(RECOVERY_RESTART_LABEL.to_owned())
    {
        RecoveryDialogAction::Restart
    } else {
        RecoveryDialogAction::KeepWaiting
    }
}

fn show_web_content_recovery_notice(
    app: tauri::AppHandle,
    label: String,
    generation: u64,
    failure: RecoveryFailure,
) {
    let (event, title, message) = match failure {
        RecoveryFailure::ReadinessDeadline => (
            "recovery_still_waiting",
            "Tyde recovery is still waiting",
            "Tyde reloaded its interface, but background or system scheduling may have delayed readiness. Tyde will keep waiting without restarting or quitting.",
        ),
        RecoveryFailure::ReloadFailed => (
            "recovery_failed",
            "Tyde recovery failed",
            "Tyde could not reload its interface after the web content process stopped. You can restart Tyde, or keep waiting without exiting.",
        ),
        RecoveryFailure::RepeatedTermination => (
            "recovery_failed",
            "Tyde recovery stopped",
            "Tyde's interface stopped again before recovery completed. You can restart Tyde, or keep waiting without exiting.",
        ),
        RecoveryFailure::AttemptLimit => (
            "recovery_failed",
            "Tyde recovery stopped",
            "Tyde's interface stopped repeatedly. Automatic recovery has been disabled. You can restart Tyde, or keep waiting without exiting.",
        ),
    };
    match failure {
        RecoveryFailure::ReadinessDeadline => tracing::warn!(
            "webview.recovery event={event} label={label} generation={generation} failure={failure:?} action=prompt"
        ),
        RecoveryFailure::ReloadFailed
        | RecoveryFailure::RepeatedTermination
        | RecoveryFailure::AttemptLimit => tracing::error!(
            "webview.recovery event={event} label={label} generation={generation} failure={failure:?} action=prompt"
        ),
    }
    let mut dialog = app
        .dialog()
        .message(message)
        .title(title)
        .kind(match failure {
            RecoveryFailure::ReadinessDeadline => MessageDialogKind::Warning,
            RecoveryFailure::ReloadFailed
            | RecoveryFailure::RepeatedTermination
            | RecoveryFailure::AttemptLimit => MessageDialogKind::Error,
        })
        .buttons(recovery_dialog_buttons(failure));
    if let Some(window) = app.get_webview_window(&label) {
        dialog = dialog.parent(&window);
    }
    dialog.show_with_result(move |result| {
        match recovery_dialog_action(failure, result) {
            RecoveryDialogAction::Restart => app.request_restart(),
            RecoveryDialogAction::KeepWaiting => tracing::info!(
                "webview.recovery event=recovery_prompt_dismissed label={label} generation={generation} failure={failure:?} action=keep_waiting"
            ),
        }
    });
}

fn schedule_recovery_readiness_deadline(
    app: tauri::AppHandle,
    label: String,
    ticket: RecoveryDeadlineTicket,
    recovery: Arc<Mutex<WebContentRecoveryPolicies>>,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(RECOVERY_READINESS_DEADLINE).await;
        let app_for_dialog = app.clone();
        let label_for_dialog = label.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let native_window_obscured = app_for_dialog
                .get_webview_window(&label_for_dialog)
                .is_some_and(|window| {
                    window.is_visible().is_ok_and(|visible| !visible)
                        || window.is_focused().is_ok_and(|focused| !focused)
                });
            let failure = with_recovery_policies(&recovery, |policies| {
                let policy = policies.for_label(&label_for_dialog);
                if native_window_obscured {
                    policy.native_window_focus_changed(false);
                }
                policy.readiness_deadline_elapsed(ticket)
            });
            if let Some(failure) = failure {
                show_web_content_recovery_notice(
                    app_for_dialog,
                    label_for_dialog,
                    ticket.generation,
                    failure,
                );
            }
        }) {
            tracing::error!(
                "webview.recovery event=failure_dialog_dispatch_failed label={label} generation={} error={error}",
                ticket.generation
            );
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
    #[cfg(not(target_os = "windows"))]
    let media_result = state.voice_media.stop_for_host(&host_id);
    let disconnect_result = state.router.disconnect(host_id).await;
    #[cfg(not(target_os = "windows"))]
    media_result?;
    disconnect_result
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
async fn send_host_frame(
    state: tauri::State<'_, ShellState>,
    host_id: String,
    envelope: String,
    binary: Vec<u8>,
) -> Result<(), String> {
    let envelope: protocol::Envelope =
        serde_json::from_str(&envelope).map_err(|error| format!("invalid host frame: {error}"))?;
    state.router.send_frame(host_id, envelope, binary)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
async fn voice_media_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
    generation: u64,
    input_only: bool,
    pending_acceptance: bool,
) -> Result<(), String> {
    state
        .voice_media
        .start(app, host_id, generation, input_only, pending_acceptance)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command(rename_all = "camelCase")]
async fn voice_media_push_output(
    state: tauri::State<'_, voice_media::NativeVoiceMedia>,
    generation: u64,
    media_seq: u64,
    timestamp_samples_48k: u64,
    opus: Vec<u8>,
) -> Result<(), String> {
    state.push_output(generation, media_seq, timestamp_samples_48k, opus)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
async fn voice_media_flush_output(
    state: tauri::State<'_, ShellState>,
    generation: u64,
) -> Result<(), String> {
    state.voice_media.flush_output(generation)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
async fn voice_media_stop(state: tauri::State<'_, ShellState>) -> Result<(), String> {
    state.voice_media.stop()
}

fn native_voice_supported(target_os: &str) -> bool {
    target_os != "windows"
}

#[tauri::command]
async fn voice_media_supported() -> Result<bool, String> {
    Ok(native_voice_supported(std::env::consts::OS))
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
    #[cfg(not(target_os = "windows"))]
    let media_result = state.voice_media.stop_for_host(&host_id);
    let _ = state.router.disconnect(host_id.clone()).await;
    #[cfg(not(target_os = "windows"))]
    media_result?;
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

    fn visible(&self) -> Option<bool> {
        match self {
            Self::Visible | Self::PageShow => Some(true),
            Self::Hidden | Self::PageHide => Some(false),
            Self::Focus | Self::Blur => None,
        }
    }
}

#[tauri::command]
fn report_frontend_lifecycle(
    webview: tauri::Webview,
    state: tauri::State<'_, ShellState>,
    event: FrontendLifecycleEvent,
) {
    let label = webview.label().to_owned();
    let (ticket, failure, generation) = event.visible().map_or((None, None, 0), |visible| {
        with_recovery_policies(&state.web_content_recovery, |policies| {
            let policy = policies.for_label(&label);
            let ticket = policy.frontend_visibility_changed(visible);
            let failure = policy.escalate_repeated_termination_if_observable();
            (ticket, failure, policy.generation)
        })
    });
    tracing::info!("webview.lifecycle event={} label={label}", event.as_str());
    if let Some(failure) = failure {
        show_web_content_recovery_notice(webview.app_handle().clone(), label, generation, failure);
    } else if let Some(ticket) = ticket {
        schedule_recovery_readiness_deadline(
            webview.app_handle().clone(),
            label,
            ticket,
            state.web_content_recovery.clone(),
        );
    }
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

#[cfg(not(target_os = "windows"))]
fn production_invoke_handler<R, F>(
    other_handler: F,
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
where
    R: tauri::Runtime,
    F: Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static,
{
    let voice_output_handler: Box<dyn Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync> =
        Box::new(tauri::generate_handler![voice_media_push_output]);
    move |invoke| {
        if invoke.message.command() == "voice_media_push_output" {
            voice_output_handler(invoke)
        } else {
            other_handler(invoke)
        }
    }
}

#[cfg(target_os = "windows")]
fn production_invoke_handler<R, F>(
    other_handler: F,
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
where
    R: tauri::Runtime,
    F: Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static,
{
    other_handler
}

#[cfg(not(target_os = "windows"))]
macro_rules! other_production_invoke_handler {
    () => {
        tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line,
            send_host_frame,
            voice_media_start,
            voice_media_flush_output,
            voice_media_stop,
            voice_media_supported,
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
        ]
    };
}

#[cfg(target_os = "windows")]
macro_rules! other_production_invoke_handler {
    () => {
        tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line,
            send_host_frame,
            voice_media_supported,
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
        ]
    };
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
    let recovery_for_window = web_content_recovery.clone();
    let recovery_for_setup = web_content_recovery.clone();

    let builder = tauri::Builder::default()
        .plugin(external_link_guard())
        .plugin(tauri_plugin_dialog::init())
        .on_page_load(move |webview, payload| {
            let label = webview.label().to_owned();
            let (generation, ticket) =
                with_recovery_policies(&recovery_for_page_load, |policies| {
                    let policy = policies.for_label(&label);
                    let ticket = match payload.event() {
                        PageLoadEvent::Started => {
                            policy.page_load_started();
                            policy.start_readiness_deadline(policy.generation)
                        }
                        PageLoadEvent::Finished => {
                            policy.page_load_finished();
                            None
                        }
                    };
                    (policy.generation, ticket)
                });
            let event = match payload.event() {
                PageLoadEvent::Started => "page_load_started",
                PageLoadEvent::Finished => "page_load_finished",
            };
            tracing::info!(
                "webview.lifecycle event={event} label={} generation={generation}",
                webview.label()
            );
            if let Some(ticket) = ticket {
                schedule_recovery_readiness_deadline(
                    webview.app_handle().clone(),
                    label,
                    ticket,
                    recovery_for_page_load.clone(),
                );
            }
        })
        .on_window_event(move |window, event| match event {
            WindowEvent::Focused(focused) => {
                tracing::info!(
                    "webview.lifecycle event=window_focus label={} focused={focused}",
                    window.label()
                );
                let label = window.label().to_owned();
                let (ticket, failure, generation) =
                    with_recovery_policies(&recovery_for_window, |policies| {
                        let policy = policies.for_label(&label);
                        let ticket = policy.native_window_focus_changed(*focused);
                        let failure = policy.escalate_repeated_termination_if_observable();
                        (ticket, failure, policy.generation)
                    });
                if let Some(failure) = failure {
                    show_web_content_recovery_notice(
                        window.app_handle().clone(),
                        label,
                        generation,
                        failure,
                    );
                } else if let Some(ticket) = ticket {
                    schedule_recovery_readiness_deadline(
                        window.app_handle().clone(),
                        label,
                        ticket,
                        recovery_for_window.clone(),
                    );
                }
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
        builder.on_web_content_process_terminate(move |webview| {
            let label = webview.label().to_owned();
            let window = webview.window();
            let native_window_obscured = window.is_visible().is_ok_and(|visible| !visible)
                || window.is_focused().is_ok_and(|focused| !focused);
            let decision = with_recovery_policies(&recovery_for_termination, |policies| {
                let policy = policies.for_label(&label);
                policy.native_window_focus_changed(!native_window_obscured);
                policy.web_content_process_terminated(Instant::now())
            });
            match decision {
                RecoveryDecision::Reload { generation } => {
                    tracing::warn!(
                        "webview.recovery event=web_content_process_terminated label={} generation={generation} action=reload",
                        webview.label()
                    );
                    match webview.reload() {
                        Ok(()) => {
                            let ticket =
                                with_recovery_policies(&recovery_for_termination, |policies| {
                                    policies
                                        .for_label(&label)
                                        .start_readiness_deadline(generation)
                                });
                            if let Some(ticket) = ticket {
                                schedule_recovery_readiness_deadline(
                                    webview.app_handle().clone(),
                                    label,
                                    ticket,
                                    recovery_for_termination.clone(),
                                );
                            }
                        }
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
                                show_web_content_recovery_notice(
                                    webview.app_handle().clone(),
                                    label,
                                    generation,
                                    failure,
                                );
                            }
                        }
                    }
                }
                RecoveryDecision::Suppress {
                    generation,
                    terminated_while_pending,
                } => {
                    if terminated_while_pending {
                        tracing::error!(
                            "webview.recovery event=web_content_process_terminated label={} generation={generation} action=record_pending_failure",
                            webview.label()
                        );
                    } else {
                        tracing::error!(
                            "webview.recovery event=web_content_process_terminated label={} generation={generation} action=suppress_failed",
                            webview.label()
                        );
                    }
                }
                RecoveryDecision::Escalate {
                    generation,
                    failure,
                } => {
                    show_web_content_recovery_notice(
                        webview.app_handle().clone(),
                        label,
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

            let session_path = server::store::session::SessionStore::default_path()
                .map_err(std::io::Error::other)?;
            let project_path = server::store::project::ProjectStore::default_path()
                .map_err(std::io::Error::other)?;
            let settings_path = server::store::settings::HostSettingsStore::default_path()
                .map_err(std::io::Error::other)?;
            let host = server::spawn_host_with_store_paths_and_runtime_config(
                session_path,
                project_path,
                settings_path,
                server::HostRuntimeConfig::default(),
            )
            .map_err(std::io::Error::other)?;

            if let Some(addr) =
                dev_host::start_dev_host_listener(host.clone()).map_err(std::io::Error::other)?
            {
                tracing::info!("dev host listener ready at {addr}");
            }

            #[cfg(not(target_os = "windows"))]
            let voice_media =
                voice_media::NativeVoiceMedia::new().map_err(std::io::Error::other)?;
            #[cfg(not(target_os = "windows"))]
            app.manage(voice_media.clone());
            app.manage(ShellState {
                router,
                host,
                host_store,
                ui_debug,
                web_content_recovery: recovery_for_setup,
                #[cfg(not(target_os = "windows"))]
                voice_media,
            });
            Ok(())
        })
        .invoke_handler(production_invoke_handler::<tauri::Wry, _>(
            other_production_invoke_handler!(),
        ))
        .build(tauri::generate_context!())
        .expect("failed to build desktop shell");

    app.run(move |app, event| {
        if let RunEvent::ExitRequested { code, api, .. } = event {
            #[cfg(not(target_os = "windows"))]
            if let Err(error) = app.state::<ShellState>().voice_media.stop() {
                tracing::error!(%error, "native audio teardown was not acknowledged");
            }
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
