use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::center_zone::{CenterWorkspaceWidth, workspace_width};
use crate::components::command_palette::{CommandPalette, execute_command, global_command_for};
use crate::components::feedback_modal::FeedbackModal;
use crate::components::header::Header;
use crate::components::help_tour::HelpTour;
use crate::components::host_browser::HostBrowser;
use crate::components::hover_popover::HoverPopover;
use crate::components::project_rail::ProjectRail;
use crate::components::settings_panel::restore_appearance;
use crate::components::workbench::Workbench;
use crate::components::workflows_panel::WorkflowRunModal;
use crate::devtools;
use crate::dispatch::dispatch_envelope;
use crate::send::send_frame;
use crate::state::{AppState, CENTER_SPLIT_RATIO_STORAGE_KEY, ConnectionStatus, SplitRatio, TabId};

use protocol::{
    Envelope, FrameKind, HelloPayload, PROTOCOL_VERSION, ProjectPath, ProjectRootPath, StreamPath,
    TYDE_VERSION,
};

fn generate_host_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    StreamPath(format!("/host/{id}"))
}

/// Yield once to the browser event loop so any pending paint/layout work
/// runs before we resume. Wasm has no real background threads; this is the
/// closest we get to "after the next frame."
async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

struct EventListenerHandle {
    window: web_sys::Window,
    event: &'static str,
    callback: wasm_bindgen::closure::Closure<dyn Fn(web_sys::Event)>,
}

impl EventListenerHandle {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            self.event,
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

struct DocumentEventListenerHandle {
    document: web_sys::Document,
    event: &'static str,
    callback: wasm_bindgen::closure::Closure<dyn Fn(web_sys::Event)>,
}

impl DocumentEventListenerHandle {
    fn remove(self) {
        let _ = self.document.remove_event_listener_with_callback(
            self.event,
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

/// A window listener registered in the **capture** phase (removal must pass the
/// same `capture` flag, hence a distinct handle type).
struct CaptureEventListenerHandle {
    window: web_sys::Window,
    event: &'static str,
    callback: wasm_bindgen::closure::Closure<dyn Fn(web_sys::Event)>,
}

impl CaptureEventListenerHandle {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback_and_bool(
            self.event,
            self.callback.as_ref().unchecked_ref(),
            true,
        );
    }
}

thread_local! {
    static APP_LISTENERS_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static APP_LISTENER_TOKEN: Cell<u64> = const { Cell::new(0) };
    static HOST_LISTENER_HANDLES: RefCell<Vec<bridge::UnlistenHandle>> = const { RefCell::new(Vec::new()) };
    static DEVTOOLS_LISTENER_HANDLE: RefCell<Option<bridge::UnlistenHandle>> = const { RefCell::new(None) };
    static KEYDOWN_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static KEYUP_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static BLUR_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static FOCUS_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static PAGE_SHOW_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static PAGE_HIDE_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static VISIBILITY_LISTENER_HANDLE: RefCell<Option<DocumentEventListenerHandle>> = const { RefCell::new(None) };
    static CLICK_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static FILE_DROP_LISTENER_HANDLES: RefCell<Vec<EventListenerHandle>> = const { RefCell::new(Vec::new()) };
    static HOVER_DISMISS_LISTENER_HANDLES: RefCell<Vec<CaptureEventListenerHandle>> = const { RefCell::new(Vec::new()) };
}

fn set_app_listeners_active(active: bool) {
    APP_LISTENERS_ACTIVE.with(|value| value.set(active));
}

fn app_listeners_active() -> bool {
    APP_LISTENERS_ACTIVE.with(Cell::get)
}

fn begin_app_listener_lifecycle() -> u64 {
    clear_app_listeners();
    APP_LISTENER_TOKEN.with(|token| {
        let next = token.get().wrapping_add(1);
        token.set(next);
        next
    })
}

fn app_listener_token_is_current(token: u64) -> bool {
    app_listeners_active() && APP_LISTENER_TOKEN.with(|current| current.get() == token)
}

/// `pub(crate)` so a test that installed the global listeners can take them down
/// again. They live in thread-locals shared by the whole wasm test binary, so a
/// leaked listener would fire against a stale `AppState` in later tests.
pub(crate) fn clear_app_listeners() {
    set_app_listeners_active(false);
    HOST_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });
    DEVTOOLS_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    KEYDOWN_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    KEYUP_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    BLUR_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    FOCUS_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    PAGE_SHOW_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    PAGE_HIDE_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    VISIBILITY_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    CLICK_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    FILE_DROP_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });
    HOVER_DISMISS_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });
}

fn keyboard_event_is_cmd_modifier(ev: &web_sys::KeyboardEvent) -> bool {
    ev.ctrl_key() || ev.meta_key() || matches!(ev.key().as_str(), "Control" | "Meta")
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|window| window.local_storage().ok().flatten())
}

/// The split ratio is the only piece of center-zone layout that survives a
/// reload (dev-docs/32 §6). Topology, focus, tabs, and resource placement are
/// deliberately not persisted, so a cold start cannot resurrect a phantom pane
/// or a stale resource. A stored value is clamped on the way in, so a
/// hand-edited or stale entry can never produce an unusable pane.
fn restore_center_split_ratio(state: &AppState) {
    let Some(storage) = local_storage() else {
        return;
    };
    let Ok(Some(raw)) = storage.get_item(CENTER_SPLIT_RATIO_STORAGE_KEY) else {
        return;
    };
    match raw.parse::<f64>() {
        Ok(value) => state.center_split_ratio.set(SplitRatio::new(value)),
        Err(error) => {
            log::warn!("ignoring unparseable stored center split ratio {raw:?}: {error}");
        }
    }
}

fn persist_center_split_ratio(ratio: SplitRatio) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(CENTER_SPLIT_RATIO_STORAGE_KEY, &ratio.get().to_string());
    }
}

/// `pub(crate)` so component tests can install the *real* global handler rather
/// than a stand-in. The Escape interaction between this listener and a modal's own
/// keydown handler is a genuine integration, and a dialog-only fixture cannot see
/// it — which is exactly how the "Escape closes the whole Settings overlay" defect
/// survived a green test suite.
pub(crate) fn install_keydown_listener(state: AppState, workspace_width: CenterWorkspaceWidth) {
    KEYDOWN_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Ok(ev) = ev.dyn_into::<web_sys::KeyboardEvent>() else {
            return;
        };
        let ctrl_or_meta = ev.ctrl_key() || ev.meta_key();
        if keyboard_event_is_cmd_modifier(&ev) {
            state.cmd_held.set(true);
        }
        // Mid-composition keystrokes belong to the IME, not to us. Every
        // binding below is a Ctrl/Cmd chord, so none of them competes with
        // ordinary text entry in an input, textarea, or the chat composer —
        // and Command/Ctrl+Enter is deliberately *not* bound here: it stays
        // element-scoped (palette/explorer/search rows open to the side, the
        // composer sends or steers) per dev-docs/32 §12.
        if ev.is_composing() {
            return;
        }
        // The global chords are *generated* from the command table's `Global`
        // bindings — this handler cannot claim a chord the table did not
        // declare global, which is what keeps `Command/Ctrl+Enter` (the
        // composer's send/steer) out of the window scope by construction rather
        // than by discipline.
        if let Some(command) = global_command_for(&ev) {
            ev.prevent_default();
            execute_command(&state, command, workspace_width.get_untracked());
            return;
        }
        match ev.key().as_str() {
            key if ctrl_or_meta && ev.shift_key() && key.eq_ignore_ascii_case("f") => {
                ev.prevent_default();
                crate::actions::open_search_panel(&state);
            }
            "k" if ctrl_or_meta => {
                ev.prevent_default();
                state.command_palette_open.update(|v| *v = !*v);
            }
            "f" if ctrl_or_meta => {
                ev.prevent_default();
                state.find_bar_open.update(|v| *v = !*v);
            }
            "F12" if ev.shift_key() => {
                // Find-references from the caret in the focused file view.
                ev.prevent_default();
                crate::components::file_view::find_references_from_current_selection(&state);
            }
            "F12" => {
                // Go-to-definition from the caret in the focused file view.
                ev.prevent_default();
                crate::components::file_view::navigate_from_current_selection(&state);
            }
            "Escape" => {
                // Escape is the one binding here that nested handlers also claim,
                // and it dismisses one layer at a time. A component that has
                // already called `prevent_default()` has said it handled this
                // Escape — dismissing *its* layer — so the global must not go on
                // to dismiss another one underneath. Without this, Escape inside a
                // modal in Settings closed the modal and the whole Settings
                // overlay in a single keypress.
                //
                // Every layer this arm owns still closes on Escape: the command
                // palette and find bar each close themselves and are unaffected,
                // and nothing inside Settings claims Escape except a modal that is
                // dismissing itself.
                if ev.default_prevented() {
                    return;
                }
                if state.command_palette_open.get_untracked() {
                    state.command_palette_open.set(false);
                } else if state.settings_open.get_untracked() {
                    state.settings_open.set(false);
                } else if state.find_bar_open.get_untracked() {
                    state.find_bar_open.set(false);
                }
            }
            _ => {}
        }
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("keydown", callback.as_ref().unchecked_ref());
    KEYDOWN_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "keydown",
            callback,
        });
    });
}

fn install_keyup_listener(state: AppState) {
    KEYUP_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Ok(ev) = ev.dyn_into::<web_sys::KeyboardEvent>() else {
            return;
        };
        if keyboard_event_is_cmd_modifier(&ev) {
            state.cmd_held.set(ev.ctrl_key() || ev.meta_key());
        }
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("keyup", callback.as_ref().unchecked_ref());
    KEYUP_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "keyup",
            callback,
        });
    });
}

fn install_cmd_stuck_clear_listeners(state: AppState) {
    BLUR_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });
    FOCUS_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });
    PAGE_SHOW_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });
    PAGE_HIDE_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });
    VISIBILITY_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let blur_state = state.clone();
    let blur_callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        blur_state.flush_composer_drafts();
        blur_state.cmd_held.set(false);
        report_lifecycle("blur");
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("blur", blur_callback.as_ref().unchecked_ref());
    BLUR_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window: window.clone(),
            event: "blur",
            callback: blur_callback,
        });
    });

    let focus_callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        report_lifecycle("focus");
    });
    let _ =
        window.add_event_listener_with_callback("focus", focus_callback.as_ref().unchecked_ref());
    FOCUS_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window: window.clone(),
            event: "focus",
            callback: focus_callback,
        });
    });

    let page_show_callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        report_lifecycle("page_show");
    });
    let _ = window
        .add_event_listener_with_callback("pageshow", page_show_callback.as_ref().unchecked_ref());
    PAGE_SHOW_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window: window.clone(),
            event: "pageshow",
            callback: page_show_callback,
        });
    });

    let page_hide_state = state.clone();
    let page_hide_callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        page_hide_state.flush_composer_drafts();
        report_lifecycle("page_hide");
    });
    let _ = window
        .add_event_listener_with_callback("pagehide", page_hide_callback.as_ref().unchecked_ref());
    PAGE_HIDE_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "pagehide",
            callback: page_hide_callback,
        });
    });

    let document = web_sys::window().unwrap().document().unwrap();
    let visibility_state = state;
    let document_for_callback = document.clone();
    let visibility_callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_| {
        if document_for_callback.hidden() {
            visibility_state.flush_composer_drafts();
            visibility_state.cmd_held.set(false);
            report_lifecycle("hidden");
        } else {
            report_lifecycle("visible");
        }
    });
    let _ = document.add_event_listener_with_callback(
        "visibilitychange",
        visibility_callback.as_ref().unchecked_ref(),
    );
    VISIBILITY_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(DocumentEventListenerHandle {
            document,
            event: "visibilitychange",
            callback: visibility_callback,
        });
    });
    report_lifecycle(
        if web_sys::window()
            .and_then(|window| window.document())
            .is_some_and(|document| document.hidden())
        {
            "hidden"
        } else {
            "visible"
        },
    );
}

fn report_lifecycle(event: &'static str) {
    spawn_local(async move {
        if let Err(error) = bridge::report_frontend_lifecycle(event).await {
            log::warn!("failed to report frontend lifecycle event: {error}");
        }
    });
}

/// Intercept link clicks inside rendered chat messages.
///
/// - File-like hrefs are opened in the Tyde file viewer.
/// - External URLs (http/https) are opened in the system browser.
/// - No link ever navigates the webview itself.
fn install_click_listener(state: AppState) {
    CLICK_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Some(target) = ev.target() else { return };

        // Walk up from the click target to find an <a> element.
        let anchor: Option<web_sys::HtmlAnchorElement> = {
            let mut node: Option<web_sys::Element> = target.dyn_into::<web_sys::Element>().ok();
            loop {
                match node {
                    None => break None,
                    Some(el) => {
                        if let Ok(a) = el.clone().dyn_into::<web_sys::HtmlAnchorElement>() {
                            break Some(a);
                        }
                        node = el.parent_element();
                    }
                }
            }
        };

        let Some(anchor) = anchor else { return };

        // Only intercept links inside rendered chat content.
        let in_chat = anchor.closest(".chat-card-body").ok().flatten().is_some();
        if !in_chat {
            return;
        }

        let href = anchor.get_attribute("href").unwrap_or_default();
        if href.is_empty() {
            return;
        }

        ev.prevent_default();

        if is_external_href(&href) {
            spawn_local(async move {
                if let Err(err) = bridge::open_external_url(href.clone()).await {
                    log::warn!("failed to open external chat link {href}: {err}");
                }
            });
        } else {
            let roots = state
                .active_project_info_untracked()
                .map(|project| project.project.root_paths())
                .unwrap_or_default();
            if let Some(path) = resolve_chat_file_href(&href, &roots) {
                crate::actions::open_project_path(&state, path);
            } else {
                log::warn!("ignoring unsupported chat link target: {href}");
            }
        }
    });

    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("click", callback.as_ref().unchecked_ref());
    CLICK_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "click",
            callback,
        });
    });
}

/// Dismiss the code-intel hover popover on **any** press anywhere in the app
/// (left click or context menu). Registered in the capture phase so a
/// component that stops propagation — tab strips, context menus — can never
/// strand an open popover floating over unrelated content. Public within the
/// crate so wasm tests can install it against a fixture state.
pub(crate) fn install_hover_dismiss_listeners(state: AppState) {
    HOVER_DISMISS_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });

    let window = web_sys::window().unwrap();
    let mut handles = Vec::new();
    for event in ["mousedown", "contextmenu"] {
        let state = state.clone();
        let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |_ev: web_sys::Event| {
            crate::actions::dismiss_hover(&state);
        });
        let _ = window.add_event_listener_with_callback_and_bool(
            event,
            callback.as_ref().unchecked_ref(),
            true,
        );
        handles.push(CaptureEventListenerHandle {
            window: window.clone(),
            event,
            callback,
        });
    }
    HOVER_DISMISS_LISTENER_HANDLES.with(|slot| {
        *slot.borrow_mut() = handles;
    });
}

fn is_external_href(href: &str) -> bool {
    let Some((scheme, _)) = href.split_once(':') else {
        return false;
    };

    scheme.eq_ignore_ascii_case("http")
        || scheme.eq_ignore_ascii_case("https")
        || scheme.eq_ignore_ascii_case("mailto")
}

fn drag_type_is_files(value: &str) -> bool {
    value == "Files"
}

fn data_transfer_types_include_files(types: &js_sys::Array) -> bool {
    types
        .iter()
        .any(|value| value.as_string().as_deref().is_some_and(drag_type_is_files))
}

fn event_has_dragged_files(ev: &web_sys::Event) -> bool {
    let Some(drag_event) = ev.dyn_ref::<web_sys::DragEvent>() else {
        return false;
    };
    let Some(data_transfer) = drag_event.data_transfer() else {
        return false;
    };

    if data_transfer_types_include_files(&data_transfer.types()) {
        return true;
    }

    data_transfer
        .files()
        .is_some_and(|files| files.length() > 0)
}

fn install_file_drop_navigation_guard() {
    FILE_DROP_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });

    let window = web_sys::window().unwrap();
    let mut handles = Vec::new();
    for event in ["dragover", "drop"] {
        let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
            if event_has_dragged_files(&ev) {
                ev.prevent_default();
            }
        });
        let _ = window.add_event_listener_with_callback(event, callback.as_ref().unchecked_ref());
        handles.push(EventListenerHandle {
            window: window.clone(),
            event,
            callback,
        });
    }

    FILE_DROP_LISTENER_HANDLES.with(|slot| {
        slot.borrow_mut().extend(handles);
    });
}

fn resolve_chat_file_href(href: &str, project_roots: &[ProjectRootPath]) -> Option<ProjectPath> {
    let decoded = percent_decode_path(href).unwrap_or_else(|| href.to_owned());
    let normalized = normalize_file_reference(&decoded)?;

    if let Some(path) = project_path_from_absolute(&normalized, project_roots) {
        return Some(path);
    }

    if is_absolute_path(&normalized) {
        return None;
    }

    if project_roots.len() != 1 {
        return None;
    }
    let root = project_roots.first()?.clone();
    Some(ProjectPath {
        root,
        relative_path: normalized,
    })
}

fn normalize_file_reference(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_scheme = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let without_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let without_line_suffix = strip_trailing_line_suffix(without_query);
    let normalized = without_line_suffix
        .trim_start_matches("./")
        .replace('\\', "/");

    if normalized.trim().is_empty() {
        return None;
    }

    Some(normalized)
}

fn strip_trailing_line_suffix(path: &str) -> &str {
    let mut candidate = path;
    for _ in 0..2 {
        let Some((prefix, suffix)) = candidate.rsplit_once(':') else {
            break;
        };
        if suffix.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = prefix;
        } else {
            break;
        }
    }
    candidate
}

fn project_path_from_absolute(
    path: &str,
    project_roots: &[ProjectRootPath],
) -> Option<ProjectPath> {
    for root in project_roots {
        let normalized_root = root.0.replace('\\', "/");
        if path == normalized_root {
            return None;
        }

        if let Some(rest) = path.strip_prefix(&normalized_root) {
            if !rest.starts_with('/') {
                continue;
            }
            let relative_path = rest.trim_start_matches('/');
            if relative_path.is_empty() {
                return None;
            }
            return Some(ProjectPath {
                root: root.clone(),
                relative_path: relative_path.to_owned(),
            });
        }
    }

    None
}

fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        || matches!(
            path.as_bytes(),
            [drive, b':', b'/', ..] if drive.is_ascii_alphabetic()
        )
}

fn percent_decode_path(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let decoded = (decode_hex_nibble(high)? << 4) | decode_hex_nibble(low)?;
                bytes.push(decoded);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configured_host_label, connect_host_failure_message, drag_type_is_files, is_external_href,
        prepare_host_failure_message, reported_host_error_message, resolve_chat_file_href,
    };
    #[cfg(target_arch = "wasm32")]
    use crate::components::header::Header;
    use crate::state::AppState;
    use leptos::prelude::Set;
    #[cfg(target_arch = "wasm32")]
    use leptos::prelude::{GetUntracked, Update};
    use protocol::{ProjectPath, ProjectRootPath};
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen::JsCast;

    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[test]
    fn recognizes_browser_file_drag_type() {
        assert!(drag_type_is_files("Files"));
        assert!(!drag_type_is_files("text/plain"));
    }

    #[test]
    fn recognizes_external_chat_hrefs_case_insensitively() {
        assert!(is_external_href("https://example.com"));
        assert!(is_external_href("HTTP://example.com"));
        assert!(is_external_href("mailto:help@example.com"));
        assert!(!is_external_href("./src/main.rs"));
        assert!(!is_external_href("file:///tmp/outside.rs"));
    }

    #[test]
    fn resolves_absolute_file_links_with_line_numbers() {
        let roots = vec![ProjectRootPath("/Users/mike/Tyde2".to_owned())];
        let resolved =
            resolve_chat_file_href("/Users/mike/Tyde2/server/src/agent/mod.rs:366", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "server/src/agent/mod.rs".to_owned(),
            })
        );
    }

    #[test]
    fn resolves_relative_file_links_with_line_and_column_numbers() {
        let roots = vec![ProjectRootPath("/Users/mike/Tyde2".to_owned())];
        let resolved = resolve_chat_file_href("./server/src/agent/mod.rs:366:8", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "server/src/agent/mod.rs".to_owned(),
            })
        );
    }

    #[test]
    fn resolves_percent_encoded_file_urls() {
        let roots = vec![ProjectRootPath("/Users/mike/Tyde2".to_owned())];
        let resolved =
            resolve_chat_file_href("file:///Users/mike/Tyde2/docs/My%20File.md#L12", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "docs/My File.md".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_absolute_paths_outside_the_active_project() {
        let roots = vec![ProjectRootPath("/Users/mike/Tyde2".to_owned())];
        let resolved = resolve_chat_file_href("/tmp/outside.rs:12", &roots);

        assert_eq!(resolved, None);
    }

    #[test]
    fn host_failure_copy_uses_configured_label_or_neutral_fallback() {
        let state = AppState::new();
        state
            .configured_hosts
            .set(vec![crate::bridge::ConfiguredHost {
                id: "7ca-test-id".to_owned(),
                label: "Offline QA".to_owned(),
                transport: crate::bridge::HostTransportConfig::SshStdio {
                    ssh_destination: "offline.invalid".to_owned(),
                    remote_command: None,
                    lifecycle: crate::bridge::RemoteHostLifecycleConfig::ManagedTyde,
                },
                auto_connect: false,
            }]);

        let label = configured_host_label(&state, "7ca-test-id");
        assert_eq!(label.as_deref(), Some("Offline QA"));
        assert_eq!(
            prepare_host_failure_message(label.as_deref(), "release unavailable"),
            "Tyde could not prepare host “Offline QA”: release unavailable"
        );
        assert_eq!(
            connect_host_failure_message(label.as_deref(), "SSH unavailable"),
            "Tyde could not connect to host “Offline QA”: SSH unavailable"
        );
        assert_eq!(
            reported_host_error_message(label.as_deref(), "connection dropped"),
            "Host “Offline QA” reported an error. connection dropped"
        );

        assert_eq!(configured_host_label(&state, "missing"), None);
        assert_eq!(
            prepare_host_failure_message(None, "release unavailable"),
            "Tyde could not prepare this host: release unavailable"
        );
        assert_eq!(
            connect_host_failure_message(None, "SSH unavailable"),
            "Tyde could not connect to this host: SSH unavailable"
        );
        assert_eq!(
            reported_host_error_message(None, "connection dropped"),
            "A host reported an error. connection dropped"
        );
    }

    #[cfg(target_arch = "wasm32")]
    thread_local! {
        static HOST_STUB_TEST_LOCKED: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    #[cfg(target_arch = "wasm32")]
    struct HostStubTestLock;

    #[cfg(target_arch = "wasm32")]
    impl Drop for HostStubTestLock {
        fn drop(&mut self) {
            HOST_STUB_TEST_LOCKED.with(|locked| locked.set(false));
        }
    }

    #[cfg(target_arch = "wasm32")]
    async fn acquire_host_stub_test_lock() -> HostStubTestLock {
        loop {
            let acquired = HOST_STUB_TEST_LOCKED.with(|locked| {
                if locked.get() {
                    false
                } else {
                    locked.set(true);
                    true
                }
            });
            if acquired {
                return HostStubTestLock;
            }
            next_tick().await;
        }
    }

    #[cfg(target_arch = "wasm32")]
    struct WindowPropertySnapshot {
        name: &'static str,
        existed: bool,
        value: wasm_bindgen::JsValue,
    }

    #[cfg(target_arch = "wasm32")]
    impl WindowPropertySnapshot {
        fn capture(window: &web_sys::Window, name: &'static str) -> Self {
            let key = wasm_bindgen::JsValue::from_str(name);
            Self {
                name,
                existed: js_sys::Reflect::has(window, &key).unwrap_or(false),
                value: js_sys::Reflect::get(window, &key)
                    .unwrap_or(wasm_bindgen::JsValue::UNDEFINED),
            }
        }

        fn restore(&self, window: &web_sys::Window) {
            let key = wasm_bindgen::JsValue::from_str(self.name);
            if self.existed {
                let _ = js_sys::Reflect::set(window, &key, &self.value);
            } else {
                let target: &js_sys::Object = window.unchecked_ref();
                let _ = js_sys::Reflect::delete_property(target, &key);
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    struct TauriStubGuard {
        tauri: WindowPropertySnapshot,
        reject: WindowPropertySnapshot,
    }

    #[cfg(target_arch = "wasm32")]
    impl Drop for TauriStubGuard {
        fn drop(&mut self) {
            let window = web_sys::window().expect("window");
            self.reject.restore(&window);
            self.tauri.restore(&window);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn install_delayed_host_prepare_rejection() -> TauriStubGuard {
        let window = web_sys::window().expect("window");
        let tauri = WindowPropertySnapshot::capture(&window, "__TAURI__");
        let reject = WindowPropertySnapshot::capture(&window, "__qa_reject_host_prepare");
        js_sys::eval(
            r#"
            (function() {
                window.__qa_reject_host_prepare = undefined;
                window.__TAURI__ = {
                    core: {
                        invoke: function(cmd) {
                            if (cmd === "ensure_configured_host_ready") {
                                return new Promise(function(_resolve, reject) {
                                    window.__qa_reject_host_prepare = reject;
                                });
                            }
                            return Promise.resolve(null);
                        }
                    }
                };
            })();
            "#,
        )
        .expect("install delayed host preparation rejection");
        TauriStubGuard { tauri, reject }
    }

    #[cfg(target_arch = "wasm32")]
    struct UserErrorStateGuard {
        previous: Option<String>,
        container: web_sys::HtmlElement,
        restored: bool,
    }

    #[cfg(target_arch = "wasm32")]
    impl UserErrorStateGuard {
        fn capture(container: web_sys::HtmlElement) -> Self {
            Self {
                previous: crate::components::header::current_user_error(),
                container,
                restored: false,
            }
        }

        fn restore(&mut self) {
            if self.restored {
                return;
            }
            if let Some(button) = self
                .container
                .query_selector(".user-error-banner-dismiss")
                .expect("query error dismissal")
            {
                button
                    .dyn_into::<web_sys::HtmlElement>()
                    .expect("error dismissal button")
                    .click();
            }
            if let Some(previous) = self.previous.clone() {
                crate::components::header::report_user_error(previous);
            }
            self.restored = true;
        }
    }

    #[cfg(target_arch = "wasm32")]
    impl Drop for UserErrorStateGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }

    #[cfg(target_arch = "wasm32")]
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .expect("window")
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .expect("schedule tick");
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn managed_prepare_failure_keeps_captured_label_and_exact_status_contract() {
        use std::cell::Cell;
        use std::rc::Rc;

        let _test_lock = acquire_host_stub_test_lock().await;
        let _tauri_stub = install_delayed_host_prepare_rejection();
        let state = AppState::new();
        state
            .configured_hosts
            .set(vec![crate::bridge::ConfiguredHost {
                id: "7ca-test-id".to_owned(),
                label: "Offline QA".to_owned(),
                transport: crate::bridge::HostTransportConfig::SshStdio {
                    ssh_destination: "offline.invalid".to_owned(),
                    remote_command: None,
                    lifecycle: crate::bridge::RemoteHostLifecycleConfig::ManagedTyde,
                },
                auto_connect: false,
            }]);
        let document = web_sys::window()
            .expect("window")
            .document()
            .expect("document");
        let container = document
            .create_element("div")
            .expect("create container")
            .dyn_into::<web_sys::HtmlElement>()
            .expect("html container");
        document
            .body()
            .expect("document body")
            .append_child(&container)
            .expect("append container");
        let header_state = state.clone();
        let _header = leptos::mount::mount_to(container.clone(), move || {
            leptos::prelude::provide_context(header_state.clone());
            leptos::view! { <Header /> }
        });
        let mut user_error_state = UserErrorStateGuard::capture(container.clone());
        let completed = Rc::new(Cell::new(false));
        let completed_task = Rc::clone(&completed);
        let task_state = state.clone();
        wasm_bindgen_futures::spawn_local(async move {
            super::connect_one_host(task_state, "7ca-test-id".to_owned()).await;
            completed_task.set(true);
        });
        next_tick().await;

        state.configured_hosts.update(|hosts| {
            hosts[0].label = "Renamed after connect".to_owned();
        });
        let window = web_sys::window().expect("window");
        let reject = js_sys::Reflect::get(
            &window,
            &wasm_bindgen::JsValue::from_str("__qa_reject_host_prepare"),
        )
        .expect("read delayed rejection")
        .dyn_into::<js_sys::Function>()
        .expect("rejection callback");
        reject
            .call1(
                &wasm_bindgen::JsValue::UNDEFINED,
                &wasm_bindgen::JsValue::from_str("release metadata unavailable"),
            )
            .expect("reject preparation");
        for _ in 0..4 {
            next_tick().await;
        }

        assert!(
            completed.get(),
            "connection attempt must finish after rejection"
        );
        let banner = crate::components::header::current_user_error()
            .expect("preparation failure must be user-visible");
        assert!(
            banner.contains("host “Offline QA”")
                && banner.contains("release metadata unavailable")
                && !banner.contains("7ca-test-id")
                && !banner.contains("JsValue("),
            "banner must use the pre-await label and clean reason: {banner}"
        );
        assert_eq!(
            state
                .host_lifecycle_statuses
                .get_untracked()
                .get("7ca-test-id"),
            Some(&crate::bridge::RemoteHostLifecycleStatus::Error {
                message: "release metadata unavailable".to_owned(),
            })
        );
        assert_eq!(
            state.connection_statuses.get_untracked().get("7ca-test-id"),
            Some(&crate::state::ConnectionStatus::Error(
                "failed to prepare remote host: release metadata unavailable".to_owned()
            ))
        );
        assert!(
            container
                .query_selector(".user-error-banner-dismiss")
                .expect("query error dismissal")
                .is_some(),
            "reported user error must be dismissible before cleanup"
        );
        user_error_state.restore();
        assert_eq!(
            crate::components::header::current_user_error(),
            user_error_state.previous
        );
    }
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    let app_shell_ref = NodeRef::<leptos::html::Div>::new();
    let frontend_ready_sent = StoredValue::new(false);
    restore_appearance(&state);
    // Reactive wiring is installed here, at the one place that mounts and
    // therefore has an executor, rather than inside the constructor.
    #[cfg(target_arch = "wasm32")]
    state.install_browser_effects();
    provide_context(state.clone());
    // One measurement of the center workspace, shared by the center zone (which
    // measures it), the command palette, and the global shortcuts (which gate
    // split availability on it). The handle carries no signal of its own — it
    // resolves the one thread-local measurement — so parking it in the keydown
    // listener below cannot outlive anybody's reactive owner.
    let center_width = workspace_width();
    restore_center_split_ratio(&state);

    let ratio_state = state.clone();
    Effect::new(move |_| {
        persist_center_split_ratio(ratio_state.center_split_ratio.get());
    });

    let listener_token = begin_app_listener_lifecycle();
    set_app_listeners_active(true);
    on_cleanup(clear_app_listeners);

    // Pre-warm syntect AFTER first paint so the first file open / first
    // markdown render doesn't pay the ~50-200ms grammar-deserialization cost
    // synchronously. The setTimeout(0) yield lets the initial mount complete
    // before we touch the lazy statics.
    //
    // Same idea for the highlight Web Worker: spawn it eagerly here so the
    // ~700ms wasm-init in the worker context happens during idle time,
    // not when the user opens their first file. Without this the first
    // file pays the cold-start latency and shows uncoloured text for a
    // visibly-jarring second.
    spawn_local(async {
        yield_to_browser().await;
        crate::syntax_highlight::warm_up();
        let _ = crate::highlight_worker::shared();
    });
    Effect::new(move |_| {
        if app_shell_ref.get().is_none() || frontend_ready_sent.get_value() {
            return;
        }
        frontend_ready_sent.set_value(true);
        spawn_local(async {
            yield_to_browser().await;
            if let Err(error) = bridge::mark_frontend_ready().await {
                log::error!("failed to acknowledge frontend readiness: {error}");
                crate::components::header::report_user_error(format!(
                    "Tyde’s interface loaded, but the desktop shell did not acknowledge it. Restart Tyde. {error}"
                ));
            }
        });
    });

    let state_for_startup = state.clone();
    Effect::new(move |_| {
        let state = state_for_startup.clone();
        let listener_token = listener_token;
        spawn_local(async move {
            initialize_hosts(state, listener_token).await;
        });
    });

    Effect::new(move |_| {
        let listener_token = listener_token;
        spawn_local(async move {
            match devtools::install_listener().await {
                Ok(handle) => {
                    if app_listener_token_is_current(listener_token) {
                        DEVTOOLS_LISTENER_HANDLE.with(|slot| {
                            if let Some(existing) = slot.borrow_mut().replace(handle) {
                                existing.remove();
                            }
                        });
                    } else {
                        handle.remove();
                    }
                }
                Err(err) => {
                    log::error!("failed to install ui debug listener: {err}");
                }
            }
        });
    });

    // Tab LRU tracker. Whenever the active tab changes, push it to the front
    // of `tab_lru`. Any tab that falls outside `TAB_LRU_CAPACITY` will
    // unmount on the next `<For>` re-render in `CenterZone`. This is the
    // mechanism that keeps "many tabs open" cheap: only the active tab plus
    // a small hot set has live components, regardless of how many tabs the
    // user has on the strip.
    //
    // Implemented as a `Memo` so the Effect only re-fires when the active
    // tab id actually changes — not on every unrelated `center_zone`
    // mutation (rename, replace_active payload upgrade, etc.). Without
    // this gate, every tab-strip rename would re-touch `tab_lru` and
    // cascade-rerender every TabMount that subscribes to it.
    //
    // Both panes' active tabs are pinned by `AppState::mounted_tab_ids`, so a
    // tab switch in one pane can never unmount the other pane's content — the
    // LRU only decides which *inactive* tabs stay warm.
    let state_for_lru_memo = state.clone();
    let active_tab_memo: Memo<Option<TabId>> =
        Memo::new(move |_| state_for_lru_memo.center_zone.with(|cz| cz.active_tab_id()));
    let state_for_lru = state.clone();
    Effect::new(move |_| {
        if let Some(active) = active_tab_memo.get() {
            state_for_lru.bump_tab_lru(active);
        }
    });

    let state_for_keys = state.clone();
    let state_for_keyup = state.clone();
    let state_for_cmd_clear = state.clone();
    let state_for_clicks = state.clone();
    Effect::new(move |_| {
        install_keydown_listener(state_for_keys.clone(), center_width);
    });
    Effect::new(move |_| {
        install_keyup_listener(state_for_keyup.clone());
    });
    Effect::new(move |_| {
        install_cmd_stuck_clear_listeners(state_for_cmd_clear.clone());
    });
    Effect::new(move |_| {
        install_click_listener(state_for_clicks.clone());
    });
    Effect::new(move |_| {
        install_file_drop_navigation_guard();
    });
    let state_for_hover_dismiss = state.clone();
    Effect::new(move |_| {
        install_hover_dismiss_listeners(state_for_hover_dismiss.clone());
    });

    let state_for_feedback = state.clone();
    let open_feedback = move |_| {
        state_for_feedback.feedback_open.set(true);
    };

    view! {
        <div class="app-shell" node_ref=app_shell_ref>
            <Header />
            <div class="app-body">
                <ProjectRail />
                <Workbench />
            </div>
            <button
                class="feedback-fab"
                title="Send feedback"
                on:click=open_feedback
            >
                "Send feedback"
            </button>
            <CommandPalette />
            <WorkflowRunModal />
            <FeedbackModal />
            <HostBrowser />
            <HelpTour />
            <HoverPopover />
        </div>
    }
}

async fn initialize_hosts(state: AppState, listener_token: u64) {
    let handles = match install_host_listeners(state.clone()).await {
        Ok(handles) => handles,
        Err(error) => {
            log::error!("failed to install host listeners: {error}");
            crate::components::header::report_user_error(format!(
                "Tyde could not start its host event listeners. Restart Tyde. {error}"
            ));
            return;
        }
    };

    if app_listener_token_is_current(listener_token) {
        HOST_LISTENER_HANDLES.with(|slot| {
            slot.borrow_mut().extend(handles);
        });
    } else {
        for handle in handles {
            handle.remove();
        }
        return;
    }

    refresh_configured_hosts(&state).await;

    if !app_listener_token_is_current(listener_token) {
        return;
    }

    let auto_connect_hosts = state
        .configured_hosts
        .get_untracked()
        .into_iter()
        .filter(|host| {
            host.auto_connect
                || matches!(host.transport, bridge::HostTransportConfig::LocalEmbedded)
        })
        .map(|host| host.id)
        .collect::<Vec<_>>();

    for host_id in auto_connect_hosts {
        connect_one_host(state.clone(), host_id).await;
    }
}

fn configured_host_label(state: &AppState, host_id: &str) -> Option<String> {
    state.configured_hosts.with_untracked(|hosts| {
        hosts
            .iter()
            .find(|host| host.id == host_id)
            .map(|host| host.label.clone())
    })
}

fn prepare_host_failure_message(label: Option<&str>, error: &str) -> String {
    match label {
        Some(label) => format!("Tyde could not prepare host “{label}”: {error}"),
        None => format!("Tyde could not prepare this host: {error}"),
    }
}

fn connect_host_failure_message(label: Option<&str>, error: &str) -> String {
    match label {
        Some(label) => format!("Tyde could not connect to host “{label}”: {error}"),
        None => format!("Tyde could not connect to this host: {error}"),
    }
}

fn reported_host_error_message(label: Option<&str>, error: &str) -> String {
    match label {
        Some(label) => format!("Host “{label}” reported an error. {error}"),
        None => format!("A host reported an error. {error}"),
    }
}

async fn install_host_listeners(state: AppState) -> Result<Vec<bridge::UnlistenHandle>, String> {
    let mut handles = Vec::with_capacity(4);

    let line_state = state.clone();
    handles.push(
        bridge::listen_host_line(move |event| {
            match serde_json::from_str::<Envelope>(&event.line) {
                Ok(envelope) => {
                    log::trace!(
                        "host_frame_rx host={} stream={} seq={} kind={}",
                        event.host_id,
                        envelope.stream,
                        envelope.seq,
                        envelope.kind
                    );
                    dispatch_envelope(&line_state, &event.host_id, envelope)
                }
                Err(error) => {
                    log::error!(
                        "failed to parse envelope from host {}: {error}",
                        event.host_id
                    );
                    line_state.connection_statuses.update(|statuses| {
                        statuses.insert(
                            event.host_id.clone(),
                            ConnectionStatus::Error(format!(
                                "failed to parse host envelope: {error}"
                            )),
                        );
                    });
                }
            }
        })
        .await?,
    );

    let disconnect_state = state.clone();
    handles.push(
        bridge::listen_host_disconnected(move |event| {
            let reconnect_local = event.host_id == "local"
                && !matches!(
                    disconnect_state
                        .connection_statuses
                        .get_untracked()
                        .get(&event.host_id),
                    Some(ConnectionStatus::Connecting)
                );
            disconnect_state.connection_statuses.update(|statuses| {
                if !matches!(
                    statuses.get(&event.host_id),
                    Some(ConnectionStatus::Error(_))
                ) {
                    statuses.insert(event.host_id.clone(), ConnectionStatus::Disconnected);
                }
            });
            disconnect_state.clear_host_runtime(&event.host_id);
            if reconnect_local {
                let state = disconnect_state.clone();
                spawn_local(async move {
                    log::warn!("local host disconnected; reconnecting");
                    connect_one_host(state, "local".to_string()).await;
                });
            }
        })
        .await?,
    );

    let error_state = state.clone();
    handles.push(
        bridge::listen_host_error(move |event| {
            log::error!("host {} error: {}", event.host_id, event.message);
            let label = configured_host_label(&error_state, &event.host_id);
            crate::components::header::report_user_error(reported_host_error_message(
                label.as_deref(),
                &event.message,
            ));
            error_state.connection_statuses.update(|statuses| {
                statuses.insert(event.host_id, ConnectionStatus::Error(event.message));
            });
        })
        .await?,
    );

    let lifecycle_state = state.clone();
    handles.push(
        bridge::listen_host_lifecycle(move |event| {
            lifecycle_state.host_lifecycle_statuses.update(|statuses| {
                statuses.insert(event.host_id, event.status);
            });
        })
        .await?,
    );

    Ok(handles)
}

pub async fn refresh_configured_hosts(state: &AppState) {
    match bridge::list_configured_hosts().await {
        Ok(store) => {
            state.configured_hosts.set(store.hosts.clone());
            state.selected_host_id.set(store.selected_host_id.clone());
            state.connection_statuses.update(|statuses| {
                statuses.retain(|host_id, _| store.hosts.iter().any(|host| &host.id == host_id));
                for host in &store.hosts {
                    statuses
                        .entry(host.id.clone())
                        .or_insert(ConnectionStatus::Disconnected);
                }
            });
            state.host_lifecycle_statuses.update(|statuses| {
                statuses.retain(|host_id, _| store.hosts.iter().any(|host| &host.id == host_id));
            });
        }
        Err(error) => {
            log::error!("failed to load configured hosts: {error}");
            crate::components::header::report_user_error(format!(
                "Tyde could not load the configured hosts, so setup cannot continue. {error}"
            ));
        }
    }
}

pub async fn connect_one_host(state: AppState, host_id: String) {
    let target_label = configured_host_label(&state, &host_id);
    log::info!("host.connect.start host={}", host_id);
    state.connection_statuses.update(|statuses| {
        statuses.insert(host_id.clone(), ConnectionStatus::Connecting);
    });

    if is_managed_remote_host(&state, &host_id) {
        match bridge::ensure_configured_host_ready(host_id.clone()).await {
            Ok(snapshot) => {
                state.host_lifecycle_statuses.update(|statuses| {
                    statuses.insert(
                        host_id.clone(),
                        bridge::RemoteHostLifecycleStatus::Snapshot { snapshot },
                    );
                });
            }
            Err(error) => {
                log::error!("failed to prepare remote host {}: {}", host_id, error);
                crate::components::header::report_user_error(prepare_host_failure_message(
                    target_label.as_deref(),
                    &error,
                ));
                state.host_lifecycle_statuses.update(|statuses| {
                    statuses.insert(
                        host_id.clone(),
                        bridge::RemoteHostLifecycleStatus::Error {
                            message: error.clone(),
                        },
                    );
                });
                state.connection_statuses.update(|statuses| {
                    statuses.insert(
                        host_id,
                        ConnectionStatus::Error(format!("failed to prepare remote host: {error}")),
                    );
                });
                return;
            }
        }
    }

    if let Err(error) = bridge::connect_host(bridge::HostIdRequest {
        host_id: host_id.clone(),
    })
    .await
    {
        log::error!("failed to connect host {}: {}", host_id, error);
        crate::components::header::report_user_error(connect_host_failure_message(
            target_label.as_deref(),
            &error,
        ));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host_id, ConnectionStatus::Error(error));
        });
        return;
    }

    let host_stream = generate_host_stream();
    state.host_streams.update(|streams| {
        streams.insert(host_id.clone(), host_stream.clone());
    });

    let hello = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        client_name: "tyde-desktop".to_owned(),
        platform: "wasm".to_owned(),
    };

    if let Err(error) = send_frame(&host_id, host_stream, FrameKind::Hello, &hello).await {
        log::error!("failed to send hello to host {}: {}", host_id, error);
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                host_id.clone(),
                ConnectionStatus::Error(format!("failed to send hello: {error}")),
            );
        });
        return;
    }
    log::info!("host.connect.done host={}", host_id);
}

pub(crate) fn is_managed_remote_host(state: &AppState, host_id: &str) -> bool {
    state
        .configured_hosts
        .get_untracked()
        .into_iter()
        .any(|host| {
            host.id == host_id
                && matches!(
                    host.transport,
                    bridge::HostTransportConfig::SshStdio {
                        lifecycle: bridge::RemoteHostLifecycleConfig::ManagedTyde,
                        ..
                    }
                )
        })
}
