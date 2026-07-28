//! Code-intelligence UI shared by the file viewer and the diff viewer: the
//! right-click action menu, and the transient notice banner's auto-clear.
//!
//! Both surfaces resolve a click to `(FileResourceKey, ProjectFileVersion, byte
//! offset)` and then offer the same two actions against it, so the menu itself
//! is surface-agnostic. What differs is *why* the actions might be unavailable:
//! the file viewer only has to consider provider status, while the diff viewer
//! also has rows that are structurally unanswerable (removed lines) or files
//! whose diff no longer matches the working tree. Callers compute their own
//! `disabled_reason` and pass it in.
//!
//! The reason is always shown rather than the actions silently doing nothing —
//! a disabled item with an explanation is the point of this menu.

use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::state::{AppState, CodeIntelKey, FileResourceKey, TabId};
use protocol::{CodeIntelState, ProjectFileVersion};

/// The resource a menu action would run against. Absent when the click did not
/// resolve to a position at all — the menu still opens, disabled, to say why.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelMenuTarget {
    pub tab_id: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
    pub offset: u32,
}

/// Where the menu opened, what it would act on, and — when the actions can't
/// run — a human reason shown as a disabled hint. `None` in the owning signal
/// means the menu is closed.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelMenuState {
    pub x: f64,
    pub y: f64,
    pub target: Option<CodeIntelMenuTarget>,
    pub disabled_reason: Option<String>,
}

struct MenuEscListener {
    window: web_sys::Window,
    callback: Closure<dyn Fn(web_sys::Event)>,
}

thread_local! {
    static MENU_ESC_LISTENER: RefCell<Option<MenuEscListener>> = const { RefCell::new(None) };
}

fn clear_menu_esc_listener() {
    MENU_ESC_LISTENER.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            let _ = handle.window.remove_event_listener_with_callback(
                "keydown",
                handle.callback.as_ref().unchecked_ref(),
            );
        }
    });
}

/// How long a transient code-intel notice stays on screen.
pub const CODE_INTEL_NOTICE_MS: i32 = 4000;

/// Auto-clear `tab_id`'s code-intel notice a few seconds after it appears.
///
/// The notice explains a request that could not be answered ("this row was
/// removed, so it does not exist on disk"), so it is informational and must not
/// linger. Call once per surface that renders the banner; the timer is scoped to
/// the caller's reactive owner and re-arms whenever a new notice for this tab
/// lands.
pub fn install_notice_auto_clear(tab_id: TabId) {
    let notice_signal = expect_context::<AppState>().code_intel_notice;
    let timer: crate::code_intel_dom::TimeoutClosureSlot = StoredValue::new_local(None);
    on_cleanup(move || crate::code_intel_dom::clear_timeout_timer(timer));
    Effect::new(move |_| {
        let is_mine =
            notice_signal.with(|notice| notice.as_ref().is_some_and(|notice| notice.tab == tab_id));
        if !is_mine {
            return;
        }
        crate::code_intel_dom::clear_timeout_timer(timer);
        let Some(window) = web_sys::window() else {
            return;
        };
        let cb = Closure::<dyn FnMut()>::new(move || {
            notice_signal.update(|notice| {
                if notice.as_ref().is_some_and(|notice| notice.tab == tab_id) {
                    *notice = None;
                }
            });
        });
        if let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            CODE_INTEL_NOTICE_MS,
        ) {
            timer.update_value(|slot| *slot = Some((id, cb)));
        }
    });
}

/// Reason the code-intel actions are unavailable based on **provider status
/// alone**, or `None` when status is no obstacle.
///
/// Mirrors the F12 / Shift+F12 preconditions: a symbol under the pointer, and
/// code intelligence that isn't unsupported/unavailable/failed for the rendered
/// file version. Surfaces with additional preconditions check those first and
/// only fall through to this.
pub fn status_disabled_reason(
    state: &AppState,
    key: &CodeIntelKey,
    version: ProjectFileVersion,
    offset: Option<u32>,
) -> Option<String> {
    if offset.is_none() {
        return Some("Right-click on a symbol".to_owned());
    }
    state.code_intel.with_untracked(|map| {
        let file = map.get(key)?;
        // Only judge against status the server resolved for the rendered text
        // (version-equals-rendered); a mismatched version says nothing here.
        if file.rendered_version != Some(version) {
            return None;
        }
        let data = file.applied()?;
        if let Some(error) = data.error.as_ref() {
            return Some(format!("Code intelligence failed: {}", error.message));
        }
        match data.status.as_ref()?.state {
            CodeIntelState::Unsupported => {
                Some("Code intelligence unsupported for this file".to_owned())
            }
            CodeIntelState::Unavailable => Some("Code intelligence unavailable".to_owned()),
            CodeIntelState::Failed => Some("Code intelligence failed".to_owned()),
            CodeIntelState::Starting | CodeIntelState::Indexing | CodeIntelState::Ready => None,
        }
    })
}

/// Right-click menu over code: Go to Definition (F12) and Show References
/// (Shift+F12). Positioned at the click, dismissed on Escape, outside-click /
/// backdrop, or action selection. When `disabled_reason` is set the actions
/// render disabled with the reason as a hint — never a silent no-op. Reuses the
/// typed `navigate_to_definition` / `start_find_references` actions, matching
/// the keybindings exactly.
#[component]
pub fn CodeIntelContextMenu(
    menu: RwSignal<Option<CodeIntelMenuState>>,
    x: f64,
    y: f64,
    #[prop(optional_no_strip)] target: Option<CodeIntelMenuTarget>,
    #[prop(optional_no_strip)] disabled_reason: Option<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    // No target means nothing to act on, so the actions are disabled whatever
    // the caller said; a reason should always accompany that.
    let disabled = disabled_reason.is_some() || target.is_none();

    // Window keydown listener for Escape dismissal, stored in a thread_local so
    // on_cleanup can remove it with a plain fn pointer (Leptos requires the
    // cleanup callback to be Send + Sync).
    clear_menu_esc_listener();
    let esc_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if let Ok(kev) = ev.dyn_into::<web_sys::KeyboardEvent>()
            && kev.key() == "Escape"
        {
            menu.set(None);
        }
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("keydown", esc_cb.as_ref().unchecked_ref());
    MENU_ESC_LISTENER.with(|slot| {
        slot.borrow_mut().replace(MenuEscListener {
            window,
            callback: esc_cb,
        });
    });
    on_cleanup(clear_menu_esc_listener);

    let def_state = state.clone();
    let def_target = target.clone();
    let on_go_to_definition = move |_: web_sys::MouseEvent| {
        menu.set(None);
        if let Some(target) = def_target.as_ref() {
            crate::actions::navigate_to_definition(
                &def_state,
                target.tab_id,
                target.key.clone(),
                target.version,
                target.offset,
            );
        }
    };

    let ref_state = state.clone();
    let ref_target = target.clone();
    let on_show_references = move |_: web_sys::MouseEvent| {
        menu.set(None);
        if let Some(target) = ref_target.as_ref() {
            crate::actions::start_find_references(
                &ref_state,
                target.tab_id,
                target.key.clone(),
                target.version,
                target.offset,
                None,
            );
        }
    };

    view! {
        // Backdrop — catches click-outside / right-click-outside to dismiss.
        <div
            style="position: fixed; inset: 0; z-index: 1000;"
            on:click=move |_| menu.set(None)
            on:contextmenu=move |ev: web_sys::MouseEvent| {
                ev.prevent_default();
                menu.set(None);
            }
        />
        <div
            class="context-menu"
            style=format!("left: {x}px; top: {y}px;")
            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
            on:contextmenu=|ev: web_sys::MouseEvent| ev.prevent_default()
        >
            <button
                class="context-menu-item context-menu-item--with-shortcut"
                disabled=disabled
                on:click=on_go_to_definition
            >
                <span class="context-menu-label">"Go to Definition"</span>
                <span class="context-menu-shortcut">"F12"</span>
            </button>
            <button
                class="context-menu-item context-menu-item--with-shortcut"
                disabled=disabled
                on:click=on_show_references
            >
                <span class="context-menu-label">"Show References"</span>
                <span class="context-menu-shortcut">"Shift+F12"</span>
            </button>
            {disabled_reason.map(|reason| view! {
                <div class="context-menu-hint">{reason}</div>
            })}
        </div>
    }
}
