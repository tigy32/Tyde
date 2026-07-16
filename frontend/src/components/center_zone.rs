use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat_default;
use crate::app::refresh_configured_hosts;
use crate::bridge::{self, SetSelectedHostRequest};
use crate::components::agent_monitor_view::AgentMonitorView;
use crate::components::chat_view::ChatView;
use crate::components::command_palette::{
    ActionId, CommandId, binding_for, command_availability_for, conflicting_occurrence,
    execute_command, move_tab, move_tab_availability,
};
use crate::components::diff_view::ReviewableDiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::components::launch_menu::{LaunchMenuBody, SubmenuAlign};
use crate::components::review_view::ReviewCommentsSurface;
use crate::components::settings_panel::SettingsPanel;
use crate::components::workflow_view::WorkflowView;
use crate::send::send_frame;
use crate::state::{
    ActiveAgentRef, AppState, ConnectionStatus, DiffKey, FileResourceKey, PaneId, SplitRatio,
    TabContent, TabId, ToolCallId,
};

use protocol::{FrameKind, ProjectId, SetAgentNamePayload};

/// Minimum width of one editor pane. Enforced three independent ways
/// (dev-docs/32 §11): this constant feeds the split-availability check and the
/// narrow-mode switch, `.editor-pane { min-width }` protects ordinary flex
/// layout and window resize, and `SplitRatio` clamps every requested ratio.
pub const MIN_PANE_WIDTH: f64 = 320.0;
/// Width of the draggable divider between panes.
pub const PANE_DIVIDER_WIDTH: f64 = 5.0;
/// Center-workspace width a new split requires, divider included.
pub const MIN_SPLIT_WIDTH: f64 = MIN_PANE_WIDTH * 2.0 + PANE_DIVIDER_WIDTH;

/// Keyboard resize steps for the divider, as a share of the workspace.
const RATIO_STEP: f64 = 0.02;
const RATIO_STEP_COARSE: f64 = 0.10;

thread_local! {
    /// The one center-workspace measurement, and the one live-region message.
    ///
    /// **`ArcRwSignal`, not `RwSignal`, and this is the whole point.** An
    /// `RwSignal` is arena-allocated and *disposed with the reactive owner that
    /// created it*. The width used to be an `RwSignal` created lazily by
    /// whichever surface asked for it first, shared onwards by context, and
    /// retained by two globals — the `ResizeObserver` closure and the window
    /// keydown listener. So the instant that first surface's owner was disposed,
    /// every other holder — the palette, the explorer, the agents panel, the
    /// monitor — was left reading a dead handle:
    ///
    /// > At center_zone.rs:56, you tried to access a reactive value which was
    /// > defined at center_zone.rs:70, but it has already been disposed.
    ///
    /// A reference-counted signal has no owner to outlive. It is created once,
    /// lives for the life of the thread, and can be held by a global without
    /// borrowing anyone's lifetime.
    static WORKSPACE_WIDTH: ArcRwSignal<Option<f64>> = ArcRwSignal::new(None);
    static ANNOUNCEMENT: ArcRwSignal<String> = ArcRwSignal::new(String::new());
}

/// Measured width of the pane row, or `None` when nothing has measured it yet.
///
/// Width-dependent behavior (split availability, narrow mode) treats `None` as
/// "wide enough": a measurement that never arrives must not silently disable
/// the feature.
///
/// A *handle*, not a container: it holds no signal of its own, so a copy of it
/// can never go stale. Every copy resolves the same thread-local signal, which
/// is exactly the truth of the thing — there is one center workspace, and it has
/// one width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CenterWorkspaceWidth;

impl CenterWorkspaceWidth {
    pub fn get(self) -> Option<f64> {
        WORKSPACE_WIDTH.with(|width| width.get())
    }

    pub fn get_untracked(self) -> Option<f64> {
        WORKSPACE_WIDTH.with(|width| width.get_untracked())
    }

    pub fn set(self, width: Option<f64>) {
        WORKSPACE_WIDTH.with(|width_signal| width_signal.set(width));
    }

    /// Back to "nothing has measured this yet".
    ///
    /// The center zone calls this when it unmounts: a measurement describes a
    /// rendered workspace, so when that workspace is gone the measurement is not
    /// stale — it does not exist. Leaving the last number behind would let a
    /// torn-down narrow window disable split for whatever renders next.
    pub fn forget_measurement() {
        WORKSPACE_WIDTH.with(|width_signal| width_signal.set(None));
    }
}

impl Default for CenterWorkspaceWidth {
    /// A workspace with no measurement yet. Resets the value; it does **not**
    /// swap the signal, so anything already subscribed keeps receiving updates.
    fn default() -> Self {
        Self::forget_measurement();
        Self
    }
}

/// The workspace-width handle. Free of owners, free of context: every caller —
/// the center zone, the palette, the explorer, the agent surfaces, the global
/// shortcuts — resolves the same signal, and none of them can outlive it.
pub fn workspace_width() -> CenterWorkspaceWidth {
    CenterWorkspaceWidth
}

pub fn pane_name(pane: PaneId) -> &'static str {
    match pane {
        PaneId::Primary => "Primary",
        PaneId::Secondary => "Secondary",
    }
}

/// "Editor pane 1 of 2: main.rs" — position plus the tab it is showing.
///
/// The tab name is what makes two occurrences of one file distinguishable to a
/// screen reader: they are the same document, and the pane is the only thing
/// that tells them apart (plan §3.4).
fn pane_accessible_name(state: &AppState, pane: PaneId) -> String {
    state.center_zone.with(|center_zone| {
        let total = if center_zone.is_split() { 2 } else { 1 };
        let index = match pane {
            PaneId::Primary => 1,
            PaneId::Secondary => 2,
        };
        let showing = center_zone
            .pane_active_tab_id(pane)
            .and_then(|tab| center_zone.tab(tab))
            .map(|tab| tab.label.clone())
            .unwrap_or_else(|| "empty".to_owned());
        format!("Editor pane {index} of {total}: {showing}")
    })
}

thread_local! {
    /// Monotonic source of DOM ids for menu reason text. Two menus can be open
    /// at once (a pane menu and a tab menu), and both may list the same command
    /// — so an id derived from the command's label would collide, and
    /// `aria-describedby` would resolve to whichever element happened to come
    /// first. A counter makes the ids unique by construction.
    static NEXT_MENU_REASON_ID: Cell<u64> = const { Cell::new(0) };
}

fn next_menu_reason_id() -> String {
    NEXT_MENU_REASON_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        format!("menu-reason-{id}")
    })
}

/// The live region's message signal.
///
/// Reference-counted for the same reason as the width: refusals originate where
/// there is no reactive owner — the window keydown handler, a drag callback, a
/// panel in another module — and an owner-scoped signal parked in a global is a
/// dangling handle the moment its owner goes away.
fn announcement_signal() -> ArcRwSignal<String> {
    ANNOUNCEMENT.with(Clone::clone)
}

/// Announce a message politely.
pub fn announce(message: impl Into<String>) {
    let message = message.into();
    ANNOUNCEMENT.with(|announcement| announcement.set(message));
}

/// Drop everything that only meant something while a workspace was on screen.
/// Called when the center zone unmounts.
fn forget_rendered_workspace() {
    CenterWorkspaceWidth::forget_measurement();
    ANNOUNCEMENT.with(|announcement| announcement.set(String::new()));
}

/// Show a tab: make it the active tab *of the pane that holds it*, and focus
/// that pane. `AppState::reveal_tab` is the authoritative contract; the center
/// zone never reaches past it into the layout to activate a tab, so a tab can
/// never be activated in the wrong pane.
pub fn reveal_tab(state: &AppState, tab: TabId) -> bool {
    state.reveal_tab(tab)
}

/// An in-flight cross-pane tab drag. The source pane and tab live in this
/// typed signal rather than in `dataTransfer`, which is supplementary
/// (dev-docs/32 §10) — browsers do not expose drag data during `dragover`, so
/// a drop target could not otherwise tell whether it is the source pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabDrag {
    source: PaneId,
    tab: TabId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TabMenu {
    tab: TabId,
    x: f64,
    y: f64,
}

struct EscListenerHandle {
    window: web_sys::Window,
    callback: Closure<dyn Fn(web_sys::Event)>,
}

/// Live `ResizeObserver` plus the closure it calls. Both are kept in a
/// thread_local so `on_cleanup` can take a plain fn pointer: Leptos requires
/// cleanup closures to be `Send + Sync`, which a `wasm_bindgen::Closure` is
/// not.
struct WidthObserverHandle {
    observer: web_sys::ResizeObserver,
    #[allow(dead_code)]
    callback: Closure<dyn Fn(js_sys::Array)>,
}

thread_local! {
    static ESC_LISTENER: RefCell<Option<EscListenerHandle>> = const { RefCell::new(None) };
    static WIDTH_OBSERVER: RefCell<Option<WidthObserverHandle>> = const { RefCell::new(None) };
}

fn clear_width_observer() {
    WIDTH_OBSERVER.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.observer.disconnect();
        }
    });
}

fn clear_esc_listener() {
    ESC_LISTENER.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            let _ = handle.window.remove_event_listener_with_callback(
                "keydown",
                handle.callback.as_ref().unchecked_ref(),
            );
        }
    });
}

fn do_rename(state: AppState, tab_id: TabId, new_label: String) {
    let content = state
        .center_zone
        .with_untracked(|center_zone| center_zone.tab(tab_id).map(|tab| tab.content.clone()));
    match content {
        Some(TabContent::Chat {
            agent_ref: Some(agent_ref),
            ..
        }) => {
            let agent_info = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.host_id == agent_ref.host_id && a.agent_id == agent_ref.agent_id)
                    .cloned()
            });
            match agent_info {
                Some(agent) => {
                    let host_id = agent.host_id.clone();
                    let stream = agent.instance_stream.clone();
                    spawn_local(async move {
                        if let Err(e) = send_frame(
                            &host_id,
                            stream,
                            FrameKind::SetAgentName,
                            &SetAgentNamePayload { name: new_label },
                        )
                        .await
                        {
                            log::error!("failed to send SetAgentName: {e}");
                        }
                    });
                }
                None => {
                    log::error!("cannot rename tab {tab_id:?}: agent not found");
                }
            }
        }
        Some(_) => {
            state.rename_tab_label(tab_id, new_label);
        }
        None => {}
    }
}

fn tab_element_id(tab_id: TabId) -> String {
    format!("tab-{}", tab_id.0)
}

fn tabpanel_element_id(tab_id: TabId) -> String {
    format!("tabpanel-{}", tab_id.0)
}

/// Move DOM focus to a tab button. Tab ids are unique across panes, including
/// for two occurrences of the same file, so this always resolves one element.
fn focus_tab_element(tab_id: TabId) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Ok(Some(element)) = document.query_selector(&format!("#{}", tab_element_id(tab_id))) else {
        return;
    };
    if let Ok(element) = element.dyn_into::<web_sys::HtmlElement>() {
        let _ = element.focus();
    }
}

/// The width the panes actually share: everything except the divider.
///
/// The ratio is a share of *this*, not of the whole workspace. Charging the
/// divider to the workspace made `flex-basis: 50%` mean "half of the total",
/// which left the secondary pane 5px short — panes were 455.5 / 450.5 at
/// "50/50" (QA F2).
fn usable_width(measured: f64) -> f64 {
    (measured - PANE_DIVIDER_WIDTH).max(0.0)
}

/// The narrowest and widest primary share both panes can actually take at this
/// width, given the 320px pane minimum.
///
/// `SplitRatio`'s 20–80% is a *policy* bound; this is a *physical* one. At a
/// 911px workspace a 20% primary is not 20% — the CSS minimum silently holds the
/// pane at 320px (35%), so the separator was announcing positions it could not
/// reach and the keyboard had dead zones at both ends (QA F1). An unmeasured
/// workspace has no physical bound to impose, so the policy bound stands alone.
fn feasible_bounds(measured: Option<f64>) -> (f64, f64) {
    let Some(usable) = measured.map(usable_width).filter(|usable| *usable > 0.0) else {
        return (SplitRatio::MIN, SplitRatio::MAX);
    };
    let smallest = (MIN_PANE_WIDTH / usable).max(SplitRatio::MIN);
    let largest = (1.0 - MIN_PANE_WIDTH / usable).min(SplitRatio::MAX);
    // At exactly MIN_SPLIT_WIDTH the two coincide (both panes are at their
    // minimum). Below it there is no split to size — narrow mode owns that, and
    // the divider is not rendered at all.
    (smallest, largest.max(smallest))
}

/// The ratio the panes are *actually* rendered at: the requested one, held
/// inside what the width physically allows.
///
/// The requested value is kept as-is in state, so widening the workspace
/// restores the position the user asked for — the clamp is a property of the
/// current width, not a destructive edit.
fn rendered_ratio(requested: SplitRatio, measured: Option<f64>) -> f64 {
    let (smallest, largest) = feasible_bounds(measured);
    requested.get().clamp(smallest, largest)
}

fn as_percent(ratio: f64) -> i32 {
    (ratio * 100.0).round() as i32
}

/// Round to a precision a layout can actually use.
///
/// Repeated keyboard steps accumulated binary noise and persisted it —
/// `0.30000000000000004` ended up in local storage (QA F4). Rounding at the
/// point the value is produced keeps every stored and announced ratio exact,
/// and 2%/10% steps land on clean values so they cannot drift.
fn normalized(ratio: f64) -> f64 {
    (ratio * 10_000.0).round() / 10_000.0
}

/// Move the divider, if the move is real.
///
/// A request outside what the width allows is held at the boundary, and if that
/// leaves the panes exactly where they were, **nothing is announced**: a
/// separator that says "20 percent" while the pane does not move is worse than
/// silence (QA F1).
fn apply_ratio(state: &AppState, requested: f64, measured: Option<f64>) {
    let (smallest, largest) = feasible_bounds(measured);
    let next = normalized(requested.clamp(smallest, largest));
    let current = rendered_ratio(
        state
            .center_zone
            .with_untracked(|center_zone| center_zone.split_ratio())
            .unwrap_or_default(),
        measured,
    );
    if as_percent(next) == as_percent(current) && (next - current).abs() < 0.0005 {
        return;
    }
    state.set_split_ratio(SplitRatio::new(next));
    announce(format!("Primary pane {} percent.", as_percent(next)));
}

/// A disabled-but-visible menu item: the reason is the point, so it is both the
/// visible text and the accessible description (dev-docs/32 §12).
/// A menu item for one typed command.
///
/// An unavailable item is **not** removed and **not** `disabled`: it keeps its
/// place, stays in the tab order, and is reachable by keyboard, because a
/// control the user cannot even focus cannot tell them why it is unavailable.
/// It is marked `aria-disabled`, described by its own reason text through
/// `aria-describedby`, refuses to act, and announces that reason
/// (dev-docs/32 §12).
#[component]
fn CommandMenuItem(
    id: CommandId,
    label: &'static str,
    /// The tab this item acts on. A tab context menu is opened *for a tab* —
    /// often a background tab, or one in the other pane — so both its
    /// availability and its activation must name that tab, never "whatever is
    /// active". `None` means the focused pane's active tab (the pane menu).
    #[prop(optional)]
    target: Option<TabId>,
    #[prop(optional)] on_run: Option<Callback<()>>,
    context_menu: RwSignal<Option<TabMenu>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let width = workspace_width();
    let availability =
        Memo::new(move |_| command_availability_for(&state, id, target, width.get()));
    let disabled = move || !availability.get().is_enabled();
    let reason = move || availability.get().reason().unwrap_or_default().to_owned();
    let reason_id = next_menu_reason_id();
    let described_by = {
        let reason_id = reason_id.clone();
        move || disabled().then(|| reason_id.clone())
    };
    // Hint and matcher come from the one binding, resolved through the unified
    // action lookup — so what a menu says and what the key does cannot drift.
    let hint = binding_for(ActionId::Command(id)).map(|binding| binding.chord().hint());

    let run_state = expect_context::<AppState>();
    let activate = move || {
        if let Some(reason) = availability.get_untracked().reason() {
            announce(reason);
            return;
        }
        context_menu.set(None);
        match on_run {
            Some(callback) => callback.run(()),
            None => execute_command(&run_state, id, width.get_untracked()),
        }
    };
    let on_click = {
        let activate = activate.clone();
        move |_: web_sys::MouseEvent| activate()
    };
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            activate();
        }
    };

    view! {
        <button
            class="context-menu-item"
            role="menuitem"
            class:disabled=disabled
            aria-disabled=move || disabled().then_some("true")
            aria-describedby=described_by
            title=reason
            on:click=on_click
            on:keydown=on_keydown
        >
            <span class="context-menu-label">{label}</span>
            {hint.map(|hint| view! { <kbd class="context-menu-shortcut">{hint}</kbd> })}
            <Show when=disabled>
                <span class="context-menu-reason" id=reason_id.clone()>{reason}</span>
            </Show>
        </button>
    }
}

#[component]
fn TabContextMenu(
    tab_id: TabId,
    x: f64,
    y: f64,
    context_menu: RwSignal<Option<TabMenu>>,
    editing_tab_id: RwSignal<Option<TabId>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let is_closeable = move || {
        state
            .center_zone
            .with(|center_zone| center_zone.tab(tab_id).is_some_and(|tab| tab.closeable))
    };

    let has_closeable_to_right = move || {
        state.center_zone.with(|center_zone| {
            let Some(pane_id) = center_zone.locate_tab(tab_id) else {
                return false;
            };
            let Some(pane) = center_zone.pane(pane_id) else {
                return false;
            };
            let Some(index) = pane.tabs.iter().position(|tab| tab.id == tab_id) else {
                return false;
            };
            pane.tabs[index + 1..].iter().any(|tab| tab.closeable)
        })
    };

    // Window keydown listener for Escape dismissal — stored in thread_local so
    // on_cleanup can use a plain fn pointer (required to be Send+Sync by Leptos).
    clear_esc_listener();
    let esc_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if let Ok(kev) = ev.dyn_into::<web_sys::KeyboardEvent>()
            && kev.key() == "Escape"
        {
            context_menu.set(None);
        }
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("keydown", esc_cb.as_ref().unchecked_ref());
    ESC_LISTENER.with(|slot| {
        slot.borrow_mut().replace(EscListenerHandle {
            window,
            callback: esc_cb,
        });
    });
    on_cleanup(clear_esc_listener);

    // Keep the menu inside the viewport: opened from a tab near the right edge
    // (which a half-width split strip makes ordinary), an unclamped menu would
    // hang off-screen with its items unreachable.
    let menu_ref = NodeRef::<leptos::html::Div>::new();
    let position: RwSignal<(f64, f64)> = RwSignal::new((x, y));
    Effect::new(move |_| {
        let Some(menu) = menu_ref.get() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        let (Some(view_width), Some(view_height)) = (
            window.inner_width().ok().and_then(|value| value.as_f64()),
            window.inner_height().ok().and_then(|value| value.as_f64()),
        ) else {
            return;
        };
        let rect = menu.get_bounding_client_rect();
        const MARGIN: f64 = 8.0;
        let left = if x + rect.width() + MARGIN > view_width {
            (view_width - rect.width() - MARGIN).max(MARGIN)
        } else {
            x
        };
        let top = if y + rect.height() + MARGIN > view_height {
            (view_height - rect.height() - MARGIN).max(MARGIN)
        } else {
            y
        };
        if (left, top) != position.get_untracked() {
            position.set((left, top));
        }
    });

    view! {
        // Backdrop — catches click-outside to dismiss
        <div
            style="position: fixed; inset: 0; z-index: 1000;"
            on:click=move |_| context_menu.set(None)
            on:contextmenu=move |ev: web_sys::MouseEvent| {
                ev.prevent_default();
                context_menu.set(None);
            }
        />
        // Menu
        <div
            class="context-menu center-menu"
            role="menu"
            aria-label="Tab actions"
            node_ref=menu_ref
            style=move || {
                let (left, top) = position.get();
                format!("left: {left}px; top: {top}px;")
            }
            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
        >
            {move || is_closeable().then(|| view! {
                <button
                    class="context-menu-item"
                    role="menuitem"
                    on:click=move |_| {
                        context_menu.set(None);
                        editing_tab_id.set(Some(tab_id));
                    }
                >
                    "Rename"
                </button>
                <button
                    class="context-menu-item"
                    role="menuitem"
                    on:click=move |_| {
                        context_menu.set(None);
                        let state = expect_context::<AppState>();
                        state.close_tab(tab_id);
                    }
                >
                    "Close"
                </button>
            })}
            <button
                class="context-menu-item"
                role="menuitem"
                on:click=move |_| {
                    context_menu.set(None);
                    let state = expect_context::<AppState>();
                    state.close_other_tabs(tab_id);
                }
            >
                "Close Other Tabs"
            </button>
            {move || has_closeable_to_right().then(|| view! {
                <button
                    class="context-menu-item"
                    role="menuitem"
                    on:click=move |_| {
                        context_menu.set(None);
                        let state = expect_context::<AppState>();
                        state.close_tabs_to_right(tab_id);
                    }
                >
                    "Close Tabs to the Right"
                </button>
            })}
            <button
                class="context-menu-item"
                role="menuitem"
                on:click=move |_| {
                    context_menu.set(None);
                    let state = expect_context::<AppState>();
                    state.close_all_tabs();
                }
            >
                "Close All Tabs"
            </button>
        </div>
    }
}

/// The pane-level action menu, reachable from every pane's tab strip. Carries
/// the pane close/join commands so they are discoverable without the palette.
/// Splits and tab moves are created by dragging tabs, not from this menu.
#[component]
fn PaneActionsMenu(pane: PaneId, context_menu: RwSignal<Option<TabMenu>>) -> impl IntoView {
    let open = RwSignal::new(false);
    let state = expect_context::<AppState>();

    let close_state = expect_context::<AppState>();
    let close_this_pane = Callback::new(move |_| {
        close_state.close_pane(pane);
        open.set(false);
    });

    view! {
        <div class="pane-actions">
            <button
                class="pane-actions-trigger"
                title=format!("{} pane actions", pane_name(pane))
                aria-label=format!("{} pane actions", pane_name(pane))
                aria-haspopup="menu"
                aria-expanded=move || if open.get() { "true" } else { "false" }
                on:click=move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    // Focus the pane the menu belongs to, so its commands read
                    // the pane the user is acting on.
                    state.focus_pane(pane);
                    open.update(|value| *value = !*value);
                }
            >
                "\u{22ef}"
            </button>
            <Show when=move || open.get()>
                <div class="pane-actions-backdrop" on:click=move |_| open.set(false)></div>
                <div
                    class="context-menu center-menu pane-actions-menu"
                    role="menu"
                    aria-label="Pane actions"
                >
                    <CommandMenuItem
                        id=CommandId::CloseEditorPane
                        label="Close Editor Pane"
                        on_run=close_this_pane
                        context_menu=context_menu
                    />
                    <CommandMenuItem
                        id=CommandId::CloseOtherPane
                        label="Return to Single Pane"
                        context_menu=context_menu
                    />
                </div>
            </Show>
        </div>
    }
}

#[component]
fn TabButton(
    tab_id: TabId,
    pane: PaneId,
    context_menu: RwSignal<Option<TabMenu>>,
    editing_tab_id: RwSignal<Option<TabId>>,
    drag: RwSignal<Option<TabDrag>>,
    drop_target: RwSignal<Option<PaneId>>,
    drag_conflict: RwSignal<Option<TabId>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let tab_data = move || {
        state
            .center_zone
            .with(|center_zone| center_zone.tab(tab_id).cloned())
    };
    // "Active" is per pane: each pane keeps its own active tab, and both stay
    // rendered. Pane focus decides which one the user is working in.
    let is_active = move || {
        state
            .center_zone
            .with(|center_zone| center_zone.pane_active_tab_id(pane) == Some(tab_id))
    };
    let is_split = move || state.center_zone.with(|center_zone| center_zone.is_split());
    let is_closeable = move || tab_data().is_some_and(|t| t.closeable);
    let is_home_tab = move || tab_data().is_some_and(|t| matches!(t.content, TabContent::Home));
    let is_editing = move || editing_tab_id.get() == Some(tab_id);

    let pane_tab_ids = move || {
        state.center_zone.with(|center_zone| {
            center_zone
                .pane(pane)
                .map(|pane| pane.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };

    // Clicking a tab reveals it: active in its own pane, and that pane focused.
    let on_click = move |_| {
        reveal_tab(&state, tab_id);
    };

    let on_contextmenu = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        if is_home_tab() {
            return;
        }
        context_menu.set(Some(TabMenu {
            tab: tab_id,
            x: ev.client_x() as f64,
            y: ev.client_y() as f64,
        }));
    };

    // Roving tab focus across the strip: arrows move and activate, Home/End
    // jump to the ends, Enter/Space activate (dev-docs/32 §11).
    let key_state = expect_context::<AppState>();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let ids = pane_tab_ids();
        let Some(index) = ids.iter().position(|id| *id == tab_id) else {
            return;
        };
        let next = match ev.key().as_str() {
            "ArrowRight" => (index + 1) % ids.len(),
            "ArrowLeft" => {
                if index == 0 {
                    ids.len() - 1
                } else {
                    index - 1
                }
            }
            "Home" => 0,
            "End" => ids.len() - 1,
            "Enter" | " " => {
                ev.prevent_default();
                reveal_tab(&key_state, tab_id);
                return;
            }
            _ => return,
        };
        ev.prevent_default();
        let target = ids[next];
        reveal_tab(&key_state, target);
        focus_tab_element(target);
    };

    // Cross-pane drag is move-only and needs an existing split. It never starts
    // from the close affordance or from a tab being renamed.
    let on_dragstart = move |ev: web_sys::DragEvent| {
        let from_close = ev
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.closest(".tab-close").ok().flatten())
            .is_some();
        if !is_split() || is_editing() || from_close {
            ev.prevent_default();
            return;
        }
        if let Some(transfer) = ev.data_transfer() {
            transfer.set_effect_allowed("move");
            let _ = transfer.set_data("text/plain", &tab_id.0.to_string());
        }
        drag.set(Some(TabDrag {
            source: pane,
            tab: tab_id,
        }));
    };
    // Fires for a completed drop and for an Escape cancellation alike.
    let on_dragend = move |_: web_sys::DragEvent| {
        drag.set(None);
        drop_target.set(None);
        drag_conflict.set(None);
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();
    let edit_value: RwSignal<String> = RwSignal::new(String::new());

    // Seed edit_value when editing starts (false→true transition only) and
    // focus the input once it's mounted. The two effects are deliberately
    // separate: the seeding effect must NOT subscribe to input_ref, because
    // that signal gets re-set on every element mount and would otherwise
    // clobber the user's typed value back to the original label.
    {
        let state_init = expect_context::<AppState>();
        let mut last_editing = false;
        Effect::new(move |_| {
            let editing_now = editing_tab_id.get() == Some(tab_id);
            if editing_now && !last_editing {
                let label = state_init.center_zone.with_untracked(|center_zone| {
                    center_zone
                        .tab(tab_id)
                        .map(|tab| tab.label.clone())
                        .unwrap_or_default()
                });
                edit_value.set(label);
            }
            last_editing = editing_now;
        });
    }
    Effect::new(move |_| {
        if editing_tab_id.get() == Some(tab_id)
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    view! {
        <button
            class=move || {
                let mut class = if is_active() { "tab active" } else { "tab" }.to_string();
                if is_home_tab() {
                    class.push_str(" tab-home");
                }
                // The occurrence a refused drag is pointing at.
                if drag_conflict.get() == Some(tab_id) {
                    class.push_str(" tab-drag-conflict");
                }
                class
            }
            role="tab"
            id=tab_element_id(tab_id)
            aria-selected=move || if is_active() { "true" } else { "false" }
            aria-controls=tabpanel_element_id(tab_id)
            tabindex=move || if is_active() { "0" } else { "-1" }
            draggable=move || if is_split() && !is_editing() { "true" } else { "false" }
            title=move || tab_data().map(|t| t.label).unwrap_or_default()
            aria-label=move || tab_data().map(|t| t.label).unwrap_or_default()
            data-tab-id=tab_id.0.to_string()
            on:click=on_click
            on:contextmenu=on_contextmenu
            on:keydown=on_keydown
            on:dragstart=on_dragstart
            on:dragend=on_dragend
        >
            {move || {
                if is_editing() {
                    let state_kd = expect_context::<AppState>();
                    let state_bl = expect_context::<AppState>();
                    let on_keydown = move |ev: web_sys::KeyboardEvent| {
                        ev.stop_propagation();
                        match ev.key().as_str() {
                            "Enter" => {
                                let label = edit_value.get_untracked().trim().to_string();
                                editing_tab_id.set(None);
                                if !label.is_empty() {
                                    do_rename(state_kd.clone(), tab_id, label);
                                }
                            }
                            "Escape" => editing_tab_id.set(None),
                            _ => {}
                        }
                    };
                    let on_blur = move |_: web_sys::FocusEvent| {
                        if editing_tab_id.with_untracked(|e| *e != Some(tab_id)) {
                            return;
                        }
                        let label = edit_value.get_untracked().trim().to_string();
                        editing_tab_id.set(None);
                        if !label.is_empty() {
                            do_rename(state_bl.clone(), tab_id, label);
                        }
                    };
                    view! {
                        <input
                            type="text"
                            class="tab-rename-input"
                            node_ref=input_ref
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                            prop:value=move || edit_value.get()
                            on:input=move |ev| edit_value.set(event_target_value(&ev))
                            on:keydown=on_keydown
                            on:blur=on_blur
                            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                        />
                    }.into_any()
                } else if is_home_tab() {
                    view! {
                        <span class="tab-home-icon" aria-hidden="true">
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>
                                <polyline points="9 22 9 12 15 12 15 22"/>
                            </svg>
                        </span>
                    }.into_any()
                } else {
                    view! {
                        <span class="tab-label">{move || tab_data().map(|t| t.label).unwrap_or_default()}</span>
                    }.into_any()
                }
            }}
            {move || is_closeable().then(|| {
                let on_close = move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    let state = expect_context::<AppState>();
                    state.close_tab(tab_id);
                };
                view! {
                    <span class="tab-close" on:click=on_close>
                        <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
                            <path d="M1 1L7 7M7 1L1 7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
                        </svg>
                    </span>
                }
            })}
        </button>
    }
}

/// What a `TabMount` is currently rendering. This is the tab's *resource
/// identity*, not just its variant: `replace_active` (tabs-disabled mode)
/// mutates a tab's content in place under the same `TabId`, so keying the
/// render on the bare variant left File→File and Diff→Diff replacements
/// showing the previous resource. Keying on identity remounts the view when
/// the resource changes, and leaves same-resource updates alone.
///
/// `Chat` is deliberately identity-free: a chat tab's `agent_ref` upgrades in
/// place when a draft spawns its agent, and `ChatView` re-derives that through
/// a `Signal` — remounting there would throw away the live chat's view state.
#[derive(Clone, Debug, PartialEq)]
enum TabRenderKey {
    Home,
    AgentMonitor,
    Chat,
    File(FileResourceKey),
    Diff(DiffKey),
    Comments(String, ProjectId),
    Workflow(ActiveAgentRef, ToolCallId),
    Missing,
}

/// Mount a single tab's content and toggle CSS visibility based on whether the
/// tab is active *in its own pane*. Both panes' active tabs stay mounted and
/// visible; a pane's inactive tabs stay mounted (up to the LRU hot set) but
/// hidden, which preserves scroll position, find state, and highlight caches
/// across tab switches.
#[component]
fn TabMount(tab_id: TabId, pane: PaneId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let is_active = move || {
        state
            .center_zone
            .with(|center_zone| center_zone.pane_active_tab_id(pane) == Some(tab_id))
    };

    let render_key: Memo<TabRenderKey> = Memo::new(move |_| {
        state.center_zone.with(|center_zone| {
            match center_zone.tab(tab_id).map(|tab| &tab.content) {
                Some(TabContent::Home) => TabRenderKey::Home,
                Some(TabContent::AgentMonitor) => TabRenderKey::AgentMonitor,
                Some(TabContent::Chat { .. }) => TabRenderKey::Chat,
                Some(TabContent::File { key }) => TabRenderKey::File(key.clone()),
                Some(TabContent::Diff {
                    host_id,
                    project_id,
                    root,
                    scope,
                    path,
                }) => TabRenderKey::Diff(DiffKey::new(
                    host_id.clone(),
                    project_id.clone(),
                    root.clone(),
                    *scope,
                    path.clone(),
                )),
                Some(TabContent::Comments {
                    host_id,
                    project_id,
                }) => TabRenderKey::Comments(host_id.clone(), project_id.clone()),
                Some(TabContent::Workflow {
                    agent_ref,
                    tool_call_id,
                }) => TabRenderKey::Workflow(agent_ref.clone(), tool_call_id.clone()),
                None => TabRenderKey::Missing,
            }
        })
    });

    view! {
        <div
            class="tab-mount"
            role="tabpanel"
            id=tabpanel_element_id(tab_id)
            aria-labelledby=tab_element_id(tab_id)
            style=move || if is_active() { "" } else { "display: none;" }
        >
            {move || {
                match render_key.get() {
                    TabRenderKey::Home => view! {
                        <div class="center-content-scroll">
                            <HomeView />
                        </div>
                    }.into_any(),
                    TabRenderKey::AgentMonitor => view! {
                        <AgentMonitorView />
                    }.into_any(),
                    TabRenderKey::Chat => {
                        // Per-tab agent_ref Signal — re-derives on the
                        // in-place `agent_ref` payload upgrade for "New
                        // Chat" tabs without remounting the ChatView.
                        let agent_ref_signal: Signal<Option<ActiveAgentRef>> =
                            Signal::derive(move || {
                                state.center_zone.with(|center_zone| {
                                    match center_zone.tab(tab_id).map(|tab| &tab.content) {
                                        Some(TabContent::Chat { agent_ref, .. }) => agent_ref.clone(),
                                        _ => None,
                                    }
                                })
                            });
                        // The singleton composer belongs to the composer owner,
                        // not to "the active tab": a chat beside a focused file
                        // keeps its composer (dev-docs/32 §7).
                        let composer_state = state.clone();
                        let owns_composer: Signal<bool> = Signal::derive(move || {
                            composer_state.center_zone.with(|center_zone| {
                                center_zone.composer_owner().map(|(_, owner)| owner) == Some(tab_id)
                            })
                        });
                        view! {
                            <ChatView
                                tab_id=tab_id
                                agent_ref=agent_ref_signal
                                owns_composer=owns_composer
                            />
                        }.into_any()
                    }
                    TabRenderKey::File(key) => {
                        view! { <FileView tab_id=tab_id key=key /> }.into_any()
                    }
                    TabRenderKey::Diff(key) => {
                        view! {
                            <ReviewableDiffView
                                tab_id=tab_id
                                host_id=key.host_id.clone()
                                project_id=key.project_id.clone()
                                root=key.root.clone()
                                scope=key.scope
                                path=key.path.clone()
                            />
                        }.into_any()
                    }
                    TabRenderKey::Comments(host_id, project_id) => {
                        view! { <ReviewCommentsSurface host_id=host_id project_id=project_id /> }.into_any()
                    }
                    TabRenderKey::Workflow(agent_ref, tool_call_id) => {
                        view! { <WorkflowView agent_ref=agent_ref tool_call_id=tool_call_id /> }.into_any()
                    }
                    // A tab whose content vanished is an explicit state, not a
                    // blank pane (plan §3.8).
                    TabRenderKey::Missing => view! {
                        <div class="center-content-scroll">
                            <div class="panel-empty">"This view is no longer available."</div>
                        </div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

/// The shared action cluster (host picker / Agents / New Chat). Rendered once,
/// in the primary strip, so a split does not duplicate global actions.
#[component]
fn PaneToolActions() -> impl IntoView {
    let state = expect_context::<AppState>();

    let menu_open = RwSignal::new(false);
    let menu_position = RwSignal::new(None::<(f64, f64)>);

    let is_connected_state = state.clone();
    let is_connected = Memo::new(move |_| {
        matches!(
            is_connected_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        )
    });

    let state_for_new_chat = state.clone();
    let on_new_chat = move |_| {
        begin_new_chat_default(&state_for_new_chat);
    };

    let state_for_agent_monitor = state.clone();

    // On the Home context (no active agent and no active project) a new chat's
    // host comes from `selected_host_id` — the same value the Settings host
    // picker controls. Surfacing that picker here makes the otherwise-hidden
    // choice explicit. For project/agent contexts the host is pinned by the
    // project or the existing agent, so we keep the Agents button instead.
    let is_home_state = state.clone();
    let is_home = Memo::new(move |_| {
        is_home_state.active_agent.get().is_none() && is_home_state.active_project.get().is_none()
    });

    let hosts_state = state.clone();
    let configured_hosts = Memo::new(move |_| hosts_state.configured_hosts.get());

    let selected_host_state = state.clone();
    let selected_host_id = Memo::new(move |_| {
        selected_host_state
            .selected_host_id
            .get()
            .unwrap_or_default()
    });

    let state_for_host_change = state.clone();
    let on_host_change = move |ev: web_sys::Event| {
        let select: web_sys::HtmlSelectElement = ev.target().unwrap().unchecked_into();
        let host_id = select.value();
        let state = state_for_host_change.clone();
        spawn_local(async move {
            match bridge::set_selected_host(SetSelectedHostRequest {
                host_id: Some(host_id),
            })
            .await
            {
                Ok(_) => refresh_configured_hosts(&state).await,
                Err(e) => log::error!("failed to set selected host: {e}"),
            }
        });
    };

    let on_toggle_menu = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        if menu_open.get_untracked() {
            menu_open.set(false);
            menu_position.set(None);
            return;
        }

        let Some(trigger) = ev
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            log::error!("new chat menu trigger click did not have an element target");
            return;
        };
        let Some(window) = web_sys::window() else {
            log::error!("new chat menu cannot open without a browser window");
            return;
        };
        let Some(window_width) = window.inner_width().ok().and_then(|width| width.as_f64()) else {
            log::error!("new chat menu cannot resolve window width");
            return;
        };
        let rect = trigger.get_bounding_client_rect();
        let top = rect.bottom() + 2.0;
        let right = (window_width - rect.right()).max(0.0);
        menu_position.set(Some((top, right)));
        menu_open.set(true);
    };

    let close_menu = move |_: web_sys::MouseEvent| {
        menu_open.set(false);
        menu_position.set(None);
    };

    view! {
        <>
            <Show
                when=move || is_home.get()
                fallback=move || {
                    let state_am = state_for_agent_monitor.clone();
                    let on_agent_monitor = move |_| {
                        state_am.open_tab(
                            TabContent::AgentMonitor,
                            "Agent Monitor".to_owned(),
                            true,
                        );
                    };
                    view! {
                        <button
                            class="center-tool-btn"
                            title="Open Agent Monitor"
                            aria-label="Open Agent Monitor"
                            on:click=on_agent_monitor
                        >
                            "Agents"
                        </button>
                    }
                }
            >
                <select
                    class="center-host-select"
                    title="Host for new chats"
                    aria-label="Host for new chats"
                    prop:value=move || selected_host_id.get()
                    on:change=on_host_change.clone()
                >
                    {move || configured_hosts.get().into_iter().map(|host| {
                        view! { <option value=host.id>{host.label}</option> }
                    }).collect_view()}
                </select>
            </Show>
            <div class="new-chat-split">
                <button
                    class="new-chat-btn"
                    title="New Chat"
                    disabled=move || !is_connected.get()
                    on:click=on_new_chat
                >
                    "New Chat"
                </button>
                <button
                    class="new-chat-menu-trigger"
                    title="Choose a launch profile for new chat"
                    disabled=move || !is_connected.get()
                    on:click=on_toggle_menu
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                >
                    <svg width="10" height="6" viewBox="0 0 10 6" fill="none">
                        <path d="M1 1L5 5L9 1" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                    </svg>
                </button>

                <Show when=move || menu_open.get() && menu_position.get().is_some()>
                    <div class="new-chat-backdrop" on:click=close_menu></div>
                    <div
                        class="new-chat-menu"
                        role="menu"
                        style=move || {
                            menu_position
                                .get()
                                .map(|(top, right)| {
                                    format!("top: {top}px; right: {right}px;")
                                })
                                .unwrap_or_default()
                        }
                    >
                        <LaunchMenuBody open_sig=menu_open submenu_align=SubmenuAlign::Auto />
                    </div>
                </Show>
            </div>
        </>
    }
}

/// One editor pane: its own tab strip, its own active tab, its own content
/// lifecycle. In an unsplit workspace exactly one of these renders and no pane
/// chrome appears, so the single-pane experience is unchanged.
#[component]
fn EditorPane(
    pane: PaneId,
    context_menu: RwSignal<Option<TabMenu>>,
    editing_tab_id: RwSignal<Option<TabId>>,
    drag: RwSignal<Option<TabDrag>>,
    drop_target: RwSignal<Option<PaneId>>,
    drag_conflict: RwSignal<Option<TabId>>,
    hidden: Signal<bool>,
    style: Signal<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tab_scroll_ref = NodeRef::<leptos::html::Div>::new();

    let is_split = Memo::new(move |_| state.center_zone.with(|center_zone| center_zone.is_split()));
    let is_focused = Memo::new(move |_| {
        state
            .center_zone
            .with(|center_zone| center_zone.focused_id() == pane)
    });

    let pane_tabs = move || {
        state.center_zone.with(|center_zone| {
            center_zone
                .pane(pane)
                .map(|pane| pane.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };
    let home_tab_id = move || {
        state.center_zone.with(|center_zone| {
            center_zone.pane(pane).and_then(|pane| {
                pane.tabs
                    .iter()
                    .find(|tab| matches!(tab.content, TabContent::Home))
                    .map(|tab| tab.id)
            })
        })
    };
    let scroll_tab_ids = move || {
        state.center_zone.with(|center_zone| {
            center_zone
                .pane(pane)
                .map(|pane| {
                    pane.tabs
                        .iter()
                        .filter(|tab| !matches!(tab.content, TabContent::Home))
                        .map(|tab| tab.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    // Content components that should exist in the DOM for this pane. Both
    // panes' active tabs are pinned by `mounted_tab_ids`, so switching tabs in
    // one pane can never unmount the other pane's content.
    let mounted_state = state.clone();
    let mounted_tab_ids = move || {
        let mounted = mounted_state.mounted_tab_ids();
        pane_tabs()
            .into_iter()
            .filter(|id| mounted.contains(id))
            .collect::<Vec<_>>()
    };

    let active_tab_id = move || {
        state
            .center_zone
            .with(|center_zone| center_zone.pane_active_tab_id(pane))
    };

    // Keep the active tab scrolled into view inside its own strip.
    Effect::new(move |_| {
        let Some(active) = active_tab_id() else {
            return;
        };
        let Some(scroller) = tab_scroll_ref.get() else {
            return;
        };

        leptos::prelude::request_animation_frame(move || {
            let selector = format!("[data-tab-id=\"{}\"]", active.0);
            let Ok(Some(tab_el)) = scroller.query_selector(&selector) else {
                return;
            };

            let scroller_rect = scroller.get_bounding_client_rect();
            let tab_rect = tab_el.get_bounding_client_rect();
            let left_delta = tab_rect.left() - scroller_rect.left();
            let right_delta = tab_rect.right() - scroller_rect.right();
            let padding = 8.0;
            let current_scroll = scroller.scroll_left();

            if left_delta < padding {
                scroller.set_scroll_left(current_scroll + (left_delta - padding).round() as i32);
            } else if right_delta > -padding {
                scroller.set_scroll_left(current_scroll + (right_delta + padding).round() as i32);
            }
        });
    });

    let tab_bar_class = move || {
        if state.tabs_enabled.get() {
            "tab-bar center-tab-bar"
        } else {
            "tab-bar center-tab-bar tab-bar-hidden"
        }
    };

    // A split halves each strip's width, so overflow is the common case rather
    // than the exception — and the strip's scrollbar is deliberately hidden.
    // Vertical wheel movement scrolls the strip horizontally (plan §4.2).
    let on_strip_wheel = move |ev: web_sys::WheelEvent| {
        let Some(scroller) = tab_scroll_ref.get_untracked() else {
            return;
        };
        let delta = if ev.delta_x().abs() > ev.delta_y().abs() {
            ev.delta_x()
        } else {
            ev.delta_y()
        };
        if delta == 0.0 {
            return;
        }
        ev.prevent_default();
        scroller.set_scroll_left(scroller.scroll_left() + delta.round() as i32);
    };

    let label_state = expect_context::<AppState>();

    let focus_state = expect_context::<AppState>();
    let on_focus_in = move |_: web_sys::FocusEvent| {
        focus_state.focus_pane(pane);
    };
    let pointer_state = expect_context::<AppState>();
    let on_pointer_down = move |_: web_sys::PointerEvent| {
        pointer_state.focus_pane(pane);
    };

    // Cross-pane drop target: move-only, other pane only, whole surface.
    //
    // A drag whose resource is already open in this pane is *not* accepted:
    // there is no `preventDefault`, so the browser shows a no-drop cursor and
    // the drop can never fire; no accepting overlay appears; the reason is
    // announced once; and the occurrence already here is highlighted, so the
    // refusal points at the thing that caused it.
    let conflict_state = expect_context::<AppState>();
    let on_dragover = move |ev: web_sys::DragEvent| {
        let Some(active_drag) = drag.get_untracked() else {
            return;
        };
        if active_drag.source == pane {
            return;
        }
        if let Some((conflict_pane, existing)) =
            conflicting_occurrence(&conflict_state, active_drag.tab)
            && conflict_pane == pane
        {
            drop_target.set(None);
            if drag_conflict.get_untracked() != Some(existing) {
                drag_conflict.set(Some(existing));
                if let Some(reason) =
                    move_tab_availability(&conflict_state, Some(active_drag.tab)).reason()
                {
                    announce(reason);
                }
            }
            return;
        }
        ev.prevent_default();
        if let Some(transfer) = ev.data_transfer() {
            transfer.set_drop_effect("move");
        }
        drag_conflict.set(None);
        drop_target.set(Some(pane));
    };
    // `dragleave` fires every time the pointer crosses into a *child* of the
    // pane — a tab, the content, a mount — so clearing the drop state on every
    // one of them makes the overlay flicker and can drop the target while the
    // pointer is still inside the pane. Only a leave whose `relatedTarget` is
    // outside this pane (or null: leaving the window entirely) is a real exit.
    let on_dragleave = move |ev: web_sys::DragEvent| {
        let still_inside = ev
            .current_target()
            .and_then(|current| current.dyn_into::<web_sys::Node>().ok())
            .zip(
                ev.related_target()
                    .and_then(|related| related.dyn_into::<web_sys::Node>().ok()),
            )
            .is_some_and(|(pane_node, related)| pane_node.contains(Some(&related)));
        if still_inside {
            return;
        }
        if drop_target.get_untracked() == Some(pane) {
            drop_target.set(None);
        }
        drag_conflict.set(None);
    };
    let drop_state = expect_context::<AppState>();
    let on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        let Some(active_drag) = drag.get_untracked() else {
            return;
        };
        drag.set(None);
        drop_target.set(None);
        drag_conflict.set(None);
        if active_drag.source == pane {
            return;
        }
        // `move_tab` announces the outcome — moved, or the state layer's own
        // reason for refusing.
        move_tab(&drop_state, pane, active_drag.tab);
    };

    view! {
        <section
            class="editor-pane"
            class:pane-focused=move || is_split.get() && is_focused.get()
            class:pane-split=move || is_split.get()
            class:pane-hidden=move || hidden.get()
            class:pane-drop-target=move || drop_target.get() == Some(pane)
            role="group"
            aria-label=move || pane_accessible_name(&label_state, pane)
            data-pane=match pane { PaneId::Primary => "primary", PaneId::Secondary => "secondary" }
            style=move || style.get()
            on:focusin=on_focus_in
            on:pointerdown=on_pointer_down
            on:dragover=on_dragover
            on:dragleave=on_dragleave
            on:drop=on_drop
        >
            <div class=tab_bar_class>
                <div
                    class="tab-strip"
                    role="tablist"
                    aria-label=format!("{} pane tabs", pane_name(pane))
                >
                    <div class="pinned-tab-leading" role="presentation">
                        {move || home_tab_id().map(|id| {
                            view! {
                                <TabButton
                                    tab_id=id
                                    pane=pane
                                    context_menu=context_menu
                                    editing_tab_id=editing_tab_id
                                    drag=drag
                                    drop_target=drop_target
                                    drag_conflict=drag_conflict
                                />
                            }
                        })}
                    </div>

                    <div
                        class="tab-strip-scroll"
                        role="presentation"
                        node_ref=tab_scroll_ref
                        on:wheel=on_strip_wheel
                    >
                        <For
                            each=move || scroll_tab_ids()
                            key=|id| *id
                            let:id
                        >
                            <TabButton
                                tab_id=id
                                pane=pane
                                context_menu=context_menu
                                editing_tab_id=editing_tab_id
                                drag=drag
                                drop_target=drop_target
                                drag_conflict=drag_conflict
                            />
                        </For>
                    </div>
                </div>

                <div class="pinned-tab-actions">
                    <span class="tab-bar-divider" aria-hidden="true"></span>
                    <PaneActionsMenu pane=pane context_menu=context_menu />
                    <Show when=move || pane == PaneId::Primary>
                        <PaneToolActions />
                    </Show>
                </div>
            </div>
            <div class="center-content">
                <For
                    each=move || mounted_tab_ids()
                    key=|id| *id
                    let:tab_id
                >
                    <TabMount tab_id=tab_id pane=pane />
                </For>
                {move || {
                    if pane_tabs().is_empty() {
                        Some(view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        })
                    } else {
                        None
                    }
                }}
                <Show when=move || drop_target.get() == Some(pane)>
                    <div class="pane-drop-overlay" aria-hidden="true"></div>
                </Show>
            </div>
        </section>
    }
}

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    let context_menu: RwSignal<Option<TabMenu>> = RwSignal::new(None);
    let editing_tab_id: RwSignal<Option<TabId>> = RwSignal::new(None);
    let drag: RwSignal<Option<TabDrag>> = RwSignal::new(None);
    let drop_target: RwSignal<Option<PaneId>> = RwSignal::new(None);
    // The occurrence already sitting in the pane a drag is hovering, if any.
    // A drag over it is refused, not accepted-then-undone.
    let drag_conflict: RwSignal<Option<TabId>> = RwSignal::new(None);
    // The live region renders the shared, owner-free announcement signal, so a
    // refusal raised anywhere — a panel, a drag callback, the window keydown
    // handler — reaches it without any of them holding a signal that dies with
    // this component.
    let announcement = announcement_signal();
    // Both of these describe a *rendered* workspace. When this one goes away the
    // measurement is not stale, it is meaningless — leaving a narrow number
    // behind would disable split for whatever renders next — and a message
    // nobody can still see must not be left in the live region.
    on_cleanup(forget_rendered_workspace);
    let dividing = RwSignal::new(false);

    let width = workspace_width();
    let panes_ref = NodeRef::<leptos::html::Div>::new();

    let is_split = Memo::new(move |_| state.center_zone.with(|center_zone| center_zone.is_split()));
    let focused_pane = Memo::new(move |_| {
        state
            .center_zone
            .with(|center_zone| center_zone.focused_id())
    });
    let requested_ratio = Memo::new(move |_| {
        state
            .center_zone
            .with(|center_zone| center_zone.split_ratio())
            .unwrap_or_default()
    });
    // What the panes are actually at — the number the separator must report.
    let ratio = Memo::new(move |_| rendered_ratio(requested_ratio.get(), width.get()));
    let ratio_percent = Memo::new(move |_| as_percent(ratio.get()));
    let bounds = Memo::new(move |_| feasible_bounds(width.get()));

    // Narrow mode: a split that no longer fits keeps both panes in state and in
    // the DOM, and shows only the focused one (dev-docs/32 §11). Widening
    // restores the side-by-side layout at the same clamped ratio.
    let narrow = Memo::new(move |_| {
        is_split.get() && width.get().is_some_and(|value| value < MIN_SPLIT_WIDTH)
    });

    // Width observer. Split availability, narrow mode, and the disabled-reason
    // text all read this one measurement of the real pane row.
    Effect::new(move |_| {
        let Some(element) = panes_ref.get() else {
            return;
        };
        let observed: web_sys::Element = element.clone().unchecked_into();
        let measure_target = observed.clone();
        width.set(Some(measure_target.get_bounding_client_rect().width()));

        let callback = Closure::<dyn Fn(js_sys::Array)>::new(move |_: js_sys::Array| {
            width.set(Some(measure_target.get_bounding_client_rect().width()));
        });
        let Ok(observer) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) else {
            return;
        };
        observer.observe(&observed);
        clear_width_observer();
        WIDTH_OBSERVER.with(|slot| {
            slot.borrow_mut()
                .replace(WidthObserverHandle { observer, callback });
        });
        on_cleanup(clear_width_observer);
    });

    // Announce topology and focus changes through one polite live region.
    let announce_state = state.clone();
    let topology = Memo::new(move |_| {
        announce_state
            .center_zone
            .with(|center_zone| (center_zone.is_split(), center_zone.focused_id()))
    });
    Effect::new(move |previous: Option<(bool, PaneId)>| {
        let current = topology.get();
        if let Some(previous) = previous
            && previous != current
        {
            let message = match (previous.0, current.0) {
                (false, true) => "Split view opened. Two editor panes.".to_owned(),
                (true, false) => "Split view closed. One editor pane.".to_owned(),
                _ => format!("{} editor pane focused.", pane_name(current.1)),
            };
            announce(message);
        }
        current
    });

    let primary_style: Signal<String> = Signal::derive(move || {
        if !is_split.get() || narrow.get() {
            String::new()
        } else {
            // The divider is subtracted *before* the split, so both panes are
            // measured against the same pool and "50/50" is 50/50. Still
            // shrinkable, so the CSS 320px minimum remains the last guard if a
            // measurement is ever stale mid-resize.
            format!(
                "flex: 0 1 calc((100% - {PANE_DIVIDER_WIDTH}px) * {});",
                ratio.get()
            )
        }
    });
    let secondary_style: Signal<String> = Signal::derive(String::new);

    let primary_hidden: Signal<bool> =
        Signal::derive(move || narrow.get() && focused_pane.get() != PaneId::Primary);
    let secondary_hidden: Signal<bool> =
        Signal::derive(move || narrow.get() && focused_pane.get() != PaneId::Secondary);

    // ── Divider ────────────────────────────────────────────────────────
    let on_pointer_down = move |ev: web_sys::PointerEvent| {
        let Some(target) = ev
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        // Pointer capture keeps the drag alive over the panes' own handlers and
        // past the window edge.
        let _ = target.set_pointer_capture(ev.pointer_id());
        dividing.set(true);
        ev.prevent_default();
    };
    let move_state = state.clone();
    let on_pointer_move = move |ev: web_sys::PointerEvent| {
        if !dividing.get_untracked() {
            return;
        }
        let Some(panes) = panes_ref.get_untracked() else {
            return;
        };
        let rect = panes.get_bounding_client_rect();
        let usable = usable_width(rect.width());
        if usable <= 0.0 {
            return;
        }
        let share = (f64::from(ev.client_x()) - rect.left()) / usable;
        apply_ratio(&move_state, share, Some(rect.width()));
    };
    let on_pointer_up = move |ev: web_sys::PointerEvent| {
        if !dividing.get_untracked() {
            return;
        }
        dividing.set(false);
        if let Some(target) = ev
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        {
            let _ = target.release_pointer_capture(ev.pointer_id());
        }
    };
    let key_state = state.clone();
    let on_divider_key = move |ev: web_sys::KeyboardEvent| {
        let measured = width.get_untracked();
        // Steps move from where the panes *are*, not from a stored value the
        // width may have overridden — otherwise the first press after a clamp
        // silently does nothing.
        let current = rendered_ratio(
            key_state
                .center_zone
                .with_untracked(|center_zone| center_zone.split_ratio())
                .unwrap_or_default(),
            measured,
        );
        let (smallest, largest) = feasible_bounds(measured);
        let step = if ev.shift_key() {
            RATIO_STEP_COARSE
        } else {
            RATIO_STEP
        };
        let next = match ev.key().as_str() {
            "ArrowLeft" => current - step,
            "ArrowRight" => current + step,
            "Home" => smallest,
            "End" => largest,
            _ => return,
        };
        ev.prevent_default();
        apply_ratio(&key_state, next, measured);
    };
    let reset_state = state.clone();
    let on_divider_dblclick = move |_: web_sys::MouseEvent| {
        apply_ratio(&reset_state, SplitRatio::DEFAULT, width.get_untracked());
    };

    let notice_primary_state = state.clone();
    let notice_secondary_state = state.clone();
    // The chords are rendered from the bindings that fire them, so a macOS user
    // is told ⌘1 rather than a Ctrl chord that does nothing on their keyboard.
    let pane_hint = |id| {
        binding_for(ActionId::Command(id))
            .map(|binding| binding.chord().hint())
            .unwrap_or_default()
    };
    let primary_hint = pane_hint(CommandId::FocusPrimaryPane);
    let secondary_hint = pane_hint(CommandId::FocusSecondaryPane);

    view! {
        <div class="center-zone">
            <Show when=move || narrow.get()>
                <div class="center-narrow-notice" role="status">
                    <span class="center-narrow-text">
                        "Two panes are open, but the workspace is too narrow to show both. \
                         Widen the window or hide a side panel to see them side by side."
                    </span>
                    <div class="center-narrow-actions">
                        <button
                            class="center-tool-btn"
                            on:click={
                                let state = notice_primary_state.clone();
                                move |_| { state.focus_pane(PaneId::Primary); }
                            }
                        >
                            {format!("Show Primary ({primary_hint})")}
                        </button>
                        <button
                            class="center-tool-btn"
                            on:click={
                                let state = notice_secondary_state.clone();
                                move |_| { state.focus_pane(PaneId::Secondary); }
                            }
                        >
                            {format!("Show Secondary ({secondary_hint})")}
                        </button>
                    </div>
                </div>
            </Show>

            // DOM order is primary strip and content, divider, secondary strip
            // and content (dev-docs/32 §11).
            <div class="center-panes" class:center-panes-narrow=move || narrow.get() node_ref=panes_ref>
                <EditorPane
                    pane=PaneId::Primary
                    context_menu=context_menu
                    editing_tab_id=editing_tab_id
                    drag=drag
                    drop_target=drop_target
                    drag_conflict=drag_conflict
                    hidden=primary_hidden
                    style=primary_style
                />
                <Show when=move || is_split.get() && !narrow.get()>
                    <div
                        class="pane-divider"
                        class:pane-divider-active=move || dividing.get()
                        role="separator"
                        tabindex="0"
                        aria-orientation="vertical"
                        aria-label="Resize editor panes"
                        // The bounds are what this width can physically reach,
                        // never a policy number the pane cannot honor.
                        aria-valuemin=move || as_percent(bounds.get().0).to_string()
                        aria-valuemax=move || as_percent(bounds.get().1).to_string()
                        aria-valuenow=move || ratio_percent.get().to_string()
                        aria-valuetext=move || format!("{} percent", ratio_percent.get())
                        on:pointerdown=on_pointer_down
                        on:pointermove=on_pointer_move.clone()
                        on:pointerup=on_pointer_up
                        on:lostpointercapture=move |_| dividing.set(false)
                        on:keydown=on_divider_key.clone()
                        on:dblclick=on_divider_dblclick.clone()
                    ></div>
                </Show>
                <Show when=move || is_split.get()>
                    <EditorPane
                        pane=PaneId::Secondary
                        context_menu=context_menu
                        editing_tab_id=editing_tab_id
                        drag=drag
                        drop_target=drop_target
                        drag_conflict=drag_conflict
                        hidden=secondary_hidden
                        style=secondary_style
                    />
                </Show>
            </div>

            <div class="visually-hidden" aria-live="polite" data-testid="center-live-region">
                {move || announcement.get()}
            </div>

            <SettingsPanel />
            {move || context_menu.get().map(|menu| {
                view! {
                    <TabContextMenu
                        tab_id=menu.tab
                        x=menu.x
                        y=menu.y
                        context_menu=context_menu
                        editing_tab_id=editing_tab_id
                    />
                }
            })}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::command_palette::CommandId;
    use crate::state::{FileResourceKey, OpenFile, TAB_LRU_CAPACITY};
    use leptos::mount::mount_to;
    use protocol::{ProjectFileVersion, ProjectPath, ProjectRootPath};
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    /// Width/height of the test mount container. Every geometry assertion is
    /// stated against these, so a layout regression that collapses the center
    /// zone (or leaves it unsized) fails instead of trivially passing.
    const CONTAINER_WIDTH: f64 = 900.0;
    const CONTAINER_HEIGHT: f64 = 700.0;
    /// `.tab-bar` height in `styles.css`.
    const TAB_BAR_HEIGHT: f64 = 36.0;
    /// `.tab-label { max-width: 140px }` in `styles.css`.
    const TAB_LABEL_MAX_WIDTH: f64 = 140.0;
    /// Minimum pointer-target size we hold every control to (WCAG 2.5.8).
    const MIN_TARGET_SIZE: f64 = 24.0;

    /// Inject the production stylesheet once per test session so layout and
    /// visibility assertions reflect real styling rather than an unstyled DOM
    /// (where every mounted tab would be visible and every box zero-sized).
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-center-zone")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-center-zone");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    /// A deterministically sized, styled container so the center zone has a
    /// real layout box: `.center-zone` is `flex: 1` inside a flex column, so
    /// without an explicitly sized parent every rect would be zero and the
    /// geometry assertions below would be vacuous.
    fn make_container() -> HtmlElement {
        make_sized_container(CONTAINER_WIDTH, CONTAINER_HEIGHT)
    }

    /// Some split behavior is a function of real width — the 320px pane
    /// minimum, split availability, and narrow mode all key off the measured
    /// workspace. Those tests size the container themselves and let the
    /// component's own width observer do the measuring.
    fn make_sized_container(width: f64, height: f64) -> HtmlElement {
        ensure_styles_loaded();
        let document = web_sys::window().unwrap().document().unwrap();
        // Every test in a wasm binary shares one document. A container left
        // behind by the previous test keeps its fixed, full-viewport box in the
        // page — where it shadows hit-testing (`elementFromPoint`) and hover for
        // whatever comes next. Each fixture disposes the last one, so it owns the
        // page it asserts about.
        let stale = document
            .query_selector_all("[data-test-container]")
            .unwrap();
        for index in 0..stale.length() {
            if let Some(node) = stale.item(index)
                && let Some(parent) = node.parent_node()
            {
                let _ = parent.remove_child(&node);
            }
        }
        let container = document.create_element("div").unwrap();
        container.set_attribute("data-test-container", "1").unwrap();
        container
            .set_attribute(
                "style",
                &format!(
                    "position: fixed; top: 0; left: 0; width: {width}px; \
                     height: {height}px; z-index: 2147483647; background: white; \
                     display: flex; flex-direction: column;"
                ),
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    /// Mirror of the tab-LRU tracker `App` installs (see `app.rs`, "Tab LRU
    /// tracker"). `CenterZone` reads `tab_lru` but never writes it, so a
    /// component-level mount must install the same Effect for tab activation
    /// to move the mounted-view set the way it does in the running app.
    fn install_tab_lru_effect(state: &AppState) {
        let state_for_memo = state.clone();
        let active_tab_memo: Memo<Option<TabId>> =
            Memo::new(move |_| state_for_memo.center_zone.with(|cz| cz.active_tab_id()));
        let state_for_lru = state.clone();
        Effect::new(move |_| {
            if let Some(active) = active_tab_memo.get() {
                state_for_lru.bump_tab_lru(active);
            }
        });
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Let the reactive runtime flush the activation → LRU Effect → `<For>`
    /// re-render chain before asserting on the DOM.
    async fn settle() {
        for _ in 0..3 {
            next_tick().await;
        }
    }

    /// Longest transition in `styles.css` on anything asserted here
    /// (`.tab` animates `color` and `border-color` for 150ms).
    const LONGEST_TRANSITION_MS: i32 = 150;

    /// Wait for CSS transitions to finish before reading a computed style.
    ///
    /// `getComputedStyle` during a transition returns the *interpolated* value —
    /// an inactive tab mid-fade reports `rgba(0, 122, 204, 0.886)`, the accent on
    /// its way out. The assertion is about the settled appearance, so the fixture
    /// waits for it rather than the assertion being loosened to accept a
    /// half-faded colour.
    async fn settle_styles() {
        settle().await;
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    &resolve,
                    LONGEST_TRANSITION_MS + 50,
                )
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn file_key(name: &str) -> FileResourceKey {
        FileResourceKey {
            host_id: "test-host".to_owned(),
            project_id: ProjectId("test-project".to_owned()),
            path: ProjectPath {
                root: ProjectRootPath("test-root".to_owned()),
                relative_path: name.to_owned(),
            },
        }
    }

    /// Seed a loaded file and open a tab for it — the same pair of steps
    /// `dispatch` performs when `ProjectFileContents` arrives.
    fn open_file_tab(state: &AppState, name: &str, contents: &str) -> FileResourceKey {
        let key = file_key(name);
        let seeded = key.clone();
        let file_contents = contents.to_owned();
        state.open_files.update(|files| {
            files.insert(
                seeded.clone(),
                OpenFile {
                    path: seeded.path.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some(file_contents),
                    is_binary: false,
                },
            );
        });
        state.open_tab(TabContent::File { key: key.clone() }, name.to_owned(), true);
        key
    }

    fn query(container: &HtmlElement, selector: &str) -> Option<HtmlElement> {
        container
            .query_selector(selector)
            .unwrap()
            .map(|el| el.dyn_into::<HtmlElement>().unwrap())
    }

    fn query_all(container: &HtmlElement, selector: &str) -> Vec<HtmlElement> {
        let nodes = container.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .map(|i| nodes.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
            .collect()
    }

    fn text_of(element: &HtmlElement) -> String {
        element.text_content().unwrap_or_default()
    }

    /// Every mounted tab content root. Tabs the LRU has evicted have no
    /// element here at all; inactive-but-mounted tabs are present and hidden.
    fn tab_mounts(container: &HtmlElement) -> Vec<HtmlElement> {
        query_all(container, ".tab-mount")
    }

    fn is_visible(element: &HtmlElement) -> bool {
        let rect = element.get_bounding_client_rect();
        rect.width() > 0.0 && rect.height() > 0.0
    }

    /// The one tab content the user can actually see. Panics unless exactly
    /// one mounted tab is visible — "exactly one" is itself the contract.
    fn visible_mount(container: &HtmlElement) -> HtmlElement {
        let visible: Vec<HtmlElement> = tab_mounts(container)
            .into_iter()
            .filter(is_visible)
            .collect();
        assert_eq!(
            visible.len(),
            1,
            "exactly one tab content should be visible at a time, found {}",
            visible.len()
        );
        visible.into_iter().next().unwrap()
    }

    /// The mounted tab (visible or hidden) whose rendered text contains
    /// `needle`, used to prove a hidden tab's view survived a tab switch.
    fn mount_containing(container: &HtmlElement, needle: &str) -> Option<HtmlElement> {
        tab_mounts(container)
            .into_iter()
            .find(|mount| text_of(mount).contains(needle))
    }

    fn tab_buttons(container: &HtmlElement) -> Vec<HtmlElement> {
        query_all(container, ".tab-bar button.tab")
    }

    fn tab_labels(container: &HtmlElement) -> Vec<String> {
        tab_buttons(container)
            .iter()
            .map(|button| button.get_attribute("aria-label").unwrap_or_default())
            .collect()
    }

    fn tab_button_named(container: &HtmlElement, label: &str) -> HtmlElement {
        tab_buttons(container)
            .into_iter()
            .find(|button| button.get_attribute("aria-label").as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no tab button named {label:?}"))
    }

    fn computed(element: &HtmlElement, property: &str) -> String {
        web_sys::window()
            .unwrap()
            .get_computed_style(element)
            .unwrap()
            .unwrap()
            .get_property_value(property)
            .unwrap()
    }

    /// Fully transparent, i.e. the inactive `.tab` bottom border.
    const TRANSPARENT: &str = "rgba(0, 0, 0, 0)";

    // ── Split-view helpers ──────────────────────────────────────────────

    fn panes(container: &HtmlElement) -> Vec<HtmlElement> {
        query_all(container, ".editor-pane")
    }

    fn pane_element(container: &HtmlElement, pane: PaneId) -> HtmlElement {
        let selector = match pane {
            PaneId::Primary => ".editor-pane[data-pane=\"primary\"]",
            PaneId::Secondary => ".editor-pane[data-pane=\"secondary\"]",
        };
        query(container, selector).unwrap_or_else(|| panic!("{selector} should be rendered"))
    }

    fn tab_buttons_in(pane: &HtmlElement) -> Vec<HtmlElement> {
        query_all(pane, "[role=\"tablist\"] button.tab")
    }

    fn tab_labels_in(pane: &HtmlElement) -> Vec<String> {
        tab_buttons_in(pane)
            .iter()
            .map(|button| button.get_attribute("aria-label").unwrap_or_default())
            .collect()
    }

    fn tab_button_named_in(pane: &HtmlElement, label: &str) -> HtmlElement {
        tab_buttons_in(pane)
            .into_iter()
            .find(|button| button.get_attribute("aria-label").as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no tab button named {label:?} in this pane"))
    }

    /// Every tab content the user can see. In a split that is one per pane.
    fn visible_mounts(container: &HtmlElement) -> Vec<HtmlElement> {
        tab_mounts(container)
            .into_iter()
            .filter(is_visible)
            .collect()
    }

    fn divider(container: &HtmlElement) -> Option<HtmlElement> {
        query(container, "[role=\"separator\"]")
    }

    fn live_region_text(container: &HtmlElement) -> String {
        query(container, "[data-testid=\"center-live-region\"]")
            .map(|region| text_of(&region))
            .unwrap_or_default()
    }

    /// Open `name` as a second file occupying the secondary pane, producing the
    /// split through the state API the UI itself uses.
    fn open_file_tab_in(
        state: &AppState,
        pane: PaneId,
        name: &str,
        contents: &str,
    ) -> FileResourceKey {
        let key = file_key(name);
        let seeded = key.clone();
        let file_contents = contents.to_owned();
        state.open_files.update(|files| {
            files.insert(
                seeded.clone(),
                OpenFile {
                    path: seeded.path.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some(file_contents),
                    is_binary: false,
                },
            );
        });
        state.open_tab_in(
            pane,
            TabContent::File { key: key.clone() },
            name.to_owned(),
            true,
        );
        key
    }

    fn key_event(key: &str, shift: bool) -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(key);
        init.set_bubbles(true);
        init.set_cancelable(true);
        init.set_shift_key(shift);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
    }

    fn pointer_event(kind: &str, client_x: i32) -> web_sys::PointerEvent {
        let init = web_sys::PointerEventInit::new();
        init.set_bubbles(true);
        init.set_cancelable(true);
        init.set_client_x(client_x);
        init.set_pointer_id(1);
        web_sys::PointerEvent::new_with_event_init_dict(kind, &init).unwrap()
    }

    /// A bubbling, cancelable drag event. Built from `MouseEventInit` because
    /// `DragEventInit` is not among the crate's enabled web-sys features; the
    /// event type is what dispatch keys off, and the handlers under test read
    /// `data_transfer()` only through an `Option`, exactly as they must for a
    /// synthetic or cross-browser drag.
    fn drag_event(kind: &str) -> web_sys::MouseEvent {
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_cancelable(true);
        web_sys::MouseEvent::new_with_mouse_event_init_dict(kind, &init).unwrap()
    }

    fn click_context_menu_item(container: &HtmlElement, text: &str) {
        let buttons = container
            .query_selector_all(".context-menu button")
            .unwrap();
        for i in 0..buttons.length() {
            let button = buttons.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            if button.text_content().as_deref().map(str::trim) == Some(text) {
                button.click();
                return;
            }
        }
        panic!("context menu item {text:?} not found");
    }

    #[wasm_bindgen_test]
    async fn chat_tab_rename_survives_external_label_update() {
        let container = make_container();
        let state = AppState::new();
        state.open_tab(TabContent::empty_chat(), "Original Chat".to_owned(), true);
        let chat_tab_id = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id().expect("chat tab active"));

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <CenterZone /> }
        });
        next_tick().await;

        let tab_button: HtmlElement = container
            .query_selector(".tab-strip-scroll button.tab")
            .unwrap()
            .expect("chat tab button")
            .dyn_into()
            .unwrap();
        let contextmenu = web_sys::MouseEvent::new("contextmenu").unwrap();
        tab_button.dispatch_event(&contextmenu).unwrap();
        next_tick().await;
        click_context_menu_item(&container, "Rename");
        next_tick().await;

        let document = web_sys::window().unwrap().document().unwrap();
        let input: HtmlInputElement = container
            .query_selector("input.tab-rename-input")
            .unwrap()
            .expect("rename input should be visible")
            .dyn_into()
            .unwrap();
        assert_eq!(
            input.value(),
            "Original Chat",
            "rename input should seed from the tab label when editing starts"
        );
        let input_node: web_sys::Element = input.clone().dyn_into().unwrap();
        let active = document.active_element().expect("focused element");
        assert!(
            active.is_same_node(Some(&input_node)),
            "rename input should be focused when editing starts"
        );

        input.set_value("User Typed Title");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        state.rename_tab_label(chat_tab_id, "External Session Label".to_owned());
        next_tick().await;

        let current_input: HtmlInputElement = container
            .query_selector("input.tab-rename-input")
            .unwrap()
            .expect("external label update must not exit rename mode")
            .dyn_into()
            .unwrap();
        let current_node: web_sys::Element = current_input.clone().dyn_into().unwrap();
        assert!(
            input_node.is_same_node(Some(&current_node)),
            "external label update remounted the rename input"
        );
        assert_eq!(
            current_input.value(),
            "User Typed Title",
            "external label update must not clobber the in-progress rename"
        );
        let active = document.active_element().expect("focused element");
        assert!(
            active.is_same_node(Some(&input_node)),
            "external label update blurred the rename input"
        );
    }

    // ── Pre-split single-pane contract ──────────────────────────────────
    //
    // The tests below pin the single-pane behavior the desktop center-split
    // work (dev-docs/32-center-split-view.md) must not regress: "Single-pane
    // behavior remains unchanged when no split exists." They deliberately
    // assert only on what a user perceives — geometry, visible content,
    // accessible names, mounted-vs-unmounted views, composer count — so the
    // split refactor can restructure `CenterZoneState` and `CenterZone`
    // internals freely as long as the unsplit experience survives.

    /// The single pane occupies the whole center workspace: tab bar on top,
    /// the active tab's content filling the rest, edge to edge. After the
    /// split lands, this is the geometry the `Single` layout must still
    /// produce (a split pane would be roughly half this width).
    #[wasm_bindgen_test]
    async fn single_pane_fills_the_center_workspace() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        let zone = query(&container, ".center-zone").expect("center zone");
        let zone_rect = zone.get_bounding_client_rect();
        assert_eq!(
            zone_rect.width(),
            CONTAINER_WIDTH,
            "center zone should fill its container's width"
        );
        assert_eq!(
            zone_rect.height(),
            CONTAINER_HEIGHT,
            "center zone should fill its container's height"
        );

        let tab_bar = query(&container, ".tab-bar").expect("tab bar");
        let tab_bar_height = tab_bar.get_bounding_client_rect().height();
        assert!(
            tab_bar_height >= TAB_BAR_HEIGHT,
            "the tab strip should be visible at its declared height when tabs are \
             enabled, got {tab_bar_height}px"
        );

        let content = query(&container, ".center-content").expect("center content");
        let content_rect = content.get_bounding_client_rect();
        assert_eq!(
            content_rect.width(),
            CONTAINER_WIDTH,
            "content area should span the full width in single-pane layout"
        );
        assert!(
            (content_rect.height() - (CONTAINER_HEIGHT - tab_bar_height)).abs() < 1.0,
            "content area should take all height below the tab bar: got {}px, \
             expected {}px",
            content_rect.height(),
            CONTAINER_HEIGHT - tab_bar_height
        );

        let mount = visible_mount(&container);
        let mount_rect = mount.get_bounding_client_rect();
        assert_eq!(
            mount_rect.width(),
            content_rect.width(),
            "the single pane's active tab should span the whole content area"
        );
        assert_eq!(
            mount_rect.height(),
            content_rect.height(),
            "the single pane's active tab should be as tall as the content area"
        );
        assert!(
            text_of(&mount).contains("Coding Agent Studio"),
            "the default active tab renders Home, got {:?}",
            text_of(&mount)
        );
    }

    /// The active tab decides what the user sees and is marked in the strip;
    /// switching tabs moves the visible content without discarding the tab
    /// left behind. Post-split this is the focused pane's active tab.
    #[wasm_bindgen_test]
    async fn active_tab_owns_the_visible_content_and_is_marked_in_the_strip() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // Open through the live component, one at a time, so the LRU tracks
        // activation exactly as it does in the running app.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        assert_eq!(
            tab_labels(&container),
            vec!["Home".to_owned(), "alpha.rs".to_owned(), "Chat".to_owned()],
            "every open tab should have a button whose accessible name is its label"
        );

        // The most recently opened tab is active: the chat is what renders.
        settle_styles().await;
        let chat_tab = tab_button_named(&container, "Chat");
        let file_tab = tab_button_named(&container, "alpha.rs");
        assert_ne!(
            computed(&chat_tab, "border-bottom-color"),
            TRANSPARENT,
            "the active tab should carry a visible active marker"
        );
        assert_eq!(
            computed(&file_tab, "border-bottom-color"),
            TRANSPARENT,
            "an inactive tab should not carry the active marker"
        );
        assert!(
            text_of(&visible_mount(&container)).contains("Send a message to start a conversation"),
            "the active chat tab's content should be the visible one"
        );

        file_tab.click();
        settle_styles().await;

        let visible = visible_mount(&container);
        assert!(
            text_of(&visible).contains("fn alpha()"),
            "clicking the file tab should show the file's contents, got {:?}",
            text_of(&visible)
        );
        assert_ne!(
            computed(&file_tab, "border-bottom-color"),
            TRANSPARENT,
            "the newly activated tab should carry the active marker"
        );
        assert_eq!(
            computed(&chat_tab, "border-bottom-color"),
            TRANSPARENT,
            "the previously active tab should drop the active marker"
        );
        assert!(
            mount_containing(&container, "Send a message to start a conversation").is_some(),
            "the deactivated chat tab should stay mounted (hidden), not be torn down"
        );
    }

    /// Closing the last closeable tab falls back to a Home tab that itself
    /// cannot be closed. dev-docs/32 keeps this exact behavior for the single
    /// pane, and forbids Home from ever seeding a second pane.
    #[wasm_bindgen_test]
    async fn closing_the_last_closeable_tab_falls_back_to_home() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        let home_tab = tab_button_named(&container, "Home");
        assert!(
            home_tab.query_selector(".tab-close").unwrap().is_none(),
            "the Home tab must not offer a close affordance"
        );

        let close = tab_button_named(&container, "Chat")
            .query_selector(".tab-close")
            .unwrap()
            .expect("a closeable tab offers a close affordance")
            .dyn_into::<HtmlElement>()
            .unwrap();
        close.click();
        settle().await;

        assert_eq!(
            tab_labels(&container),
            vec!["Home".to_owned()],
            "closing the only closeable tab should leave just Home"
        );
        assert!(
            text_of(&visible_mount(&container)).contains("Coding Agent Studio"),
            "Home should become the visible tab after the last closeable tab closes"
        );
    }

    /// Bulk close keeps the non-closeable Home tab and activates it. The
    /// split work reroutes this through an occurrence-aware teardown path;
    /// the user-visible outcome must not move.
    #[wasm_bindgen_test]
    async fn close_all_tabs_leaves_only_home_active() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        let chat_tab = tab_button_named(&container, "Chat");
        let contextmenu = web_sys::MouseEvent::new("contextmenu").unwrap();
        chat_tab.dispatch_event(&contextmenu).unwrap();
        settle().await;
        click_context_menu_item(&container, "Close All Tabs");
        settle().await;

        assert_eq!(
            tab_labels(&container),
            vec!["Home".to_owned()],
            "Close All Tabs should close every closeable tab and keep Home"
        );
        assert!(
            text_of(&visible_mount(&container)).contains("Coding Agent Studio"),
            "Home should be the visible tab after Close All Tabs"
        );
        assert_eq!(
            tab_mounts(&container).len(),
            1,
            "the closed tabs' views should be unmounted, not merely hidden"
        );
    }

    /// Exactly one chat composer exists, and it belongs to the active chat.
    /// dev-docs/32 §7 keeps the singleton composer but re-derives its owner
    /// from `composer_owner()` instead of the active tab; in an unsplit
    /// workspace the two must agree, including "no chat active → no
    /// composer".
    #[wasm_bindgen_test]
    async fn exactly_one_composer_and_it_belongs_to_the_active_chat() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        let composers = query_all(&container, ".chat-input-area");
        assert_eq!(
            composers.len(),
            1,
            "an active chat tab should mount exactly one composer"
        );
        let visible = visible_mount(&container);
        assert!(
            visible.contains(Some(&composers[0])),
            "the composer should live inside the visible (active) chat tab"
        );

        // Both tabs are still mounted here (the LRU holds two), so a second
        // composer would have to come from the hidden chat tab — it must not.
        assert!(
            mount_containing(&container, "fn alpha()").is_some(),
            "the file tab should still be mounted while the chat is active"
        );

        tab_button_named(&container, "alpha.rs").click();
        settle().await;
        assert_eq!(
            query_all(&container, ".chat-input-area").len(),
            0,
            "with a file active and no chat active, no composer should be mounted"
        );
        assert!(
            mount_containing(&container, "Send a message to start a conversation").is_some(),
            "the chat tab should stay mounted and readable while a file is active"
        );

        tab_button_named(&container, "Chat").click();
        settle().await;
        let composers = query_all(&container, ".chat-input-area");
        assert_eq!(
            composers.len(),
            1,
            "re-activating the chat should restore exactly one composer"
        );
        assert!(
            visible_mount(&container).contains(Some(&composers[0])),
            "the restored composer should belong to the newly active chat tab"
        );
    }

    /// Recently active tabs stay mounted (state preserved, view reused) while
    /// colder tabs are unmounted but keep their strip button. dev-docs/32
    /// requires every pane's active tab to remain mounted; this pins the
    /// single-pane hot-set behavior that guarantee builds on.
    #[wasm_bindgen_test]
    async fn recent_tabs_stay_mounted_and_colder_tabs_unmount() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        for (name, body) in [
            ("alpha.rs", "fn alpha() {}"),
            ("bravo.rs", "fn bravo() {}"),
            ("charlie.rs", "fn charlie() {}"),
        ] {
            open_file_tab(&state, name, body);
            settle().await;
        }

        assert_eq!(
            tab_labels(&container).len(),
            4,
            "Home plus three file tabs should all have strip buttons"
        );
        assert_eq!(
            tab_mounts(&container).len(),
            TAB_LRU_CAPACITY,
            "only the active tab plus the LRU hot set should be mounted"
        );
        assert!(
            mount_containing(&container, "fn alpha()").is_none(),
            "the coldest tab's view should be unmounted even though its tab remains"
        );

        // The hidden-but-mounted tab must be *reused* on switch-back, not
        // rebuilt: that reuse is what preserves scroll and view state.
        let bravo_mount = mount_containing(&container, "fn bravo()")
            .expect("the previously active tab should still be mounted");
        tab_button_named(&container, "bravo.rs").click();
        settle().await;
        let visible = visible_mount(&container);
        assert!(
            visible.is_same_node(Some(&bravo_mount)),
            "switching back to a mounted tab should reuse its view, not remount it"
        );
        assert!(
            text_of(&visible).contains("fn bravo()"),
            "the reused view should still render its file"
        );

        // Activating the coldest tab remounts it and evicts the least
        // recently used one — the strip is unchanged either way.
        tab_button_named(&container, "alpha.rs").click();
        settle().await;
        assert!(
            text_of(&visible_mount(&container)).contains("fn alpha()"),
            "an unmounted tab should remount from cached state when reactivated"
        );
        assert_eq!(
            tab_mounts(&container).len(),
            TAB_LRU_CAPACITY,
            "the mounted hot set should stay bounded after remounting a cold tab"
        );
        assert!(
            mount_containing(&container, "fn charlie()").is_none(),
            "the least recently used tab should have been evicted"
        );
        assert_eq!(
            tab_labels(&container).len(),
            4,
            "unmounting a tab's view must never remove its tab from the strip"
        );
    }

    /// With tabs disabled the strip is hidden, the content takes the whole
    /// workspace, and opening a resource replaces the single tab instead of
    /// adding one. dev-docs/32 §9 disables split entirely in this mode
    /// ("Enable tabs to use split view"), so this single-tab behavior must
    /// survive untouched.
    #[wasm_bindgen_test]
    async fn tabs_disabled_hides_the_strip_and_replaces_the_active_tab() {
        let container = make_container();
        let state = AppState::new();
        state.tabs_enabled.set(false);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        let tab_bar = query(&container, ".tab-bar").expect("tab bar element");
        assert_eq!(
            tab_bar.get_bounding_client_rect().height(),
            0.0,
            "the tab strip should not be visible when tabs are disabled"
        );
        let content = query(&container, ".center-content").expect("center content");
        assert_eq!(
            content.get_bounding_client_rect().height(),
            CONTAINER_HEIGHT,
            "with no strip, content should take the whole workspace height"
        );

        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;
        assert_eq!(
            tab_mounts(&container).len(),
            1,
            "tabs-disabled mode should never hold more than one tab"
        );
        let visible = visible_mount(&container);
        assert!(
            text_of(&visible).contains("Send a message to start a conversation"),
            "opening a chat should replace the visible content"
        );
        assert!(
            !text_of(&visible).contains("Coding Agent Studio"),
            "the replaced Home content should no longer render"
        );

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        assert_eq!(
            tab_mounts(&container).len(),
            1,
            "opening a file should replace the chat, not add a second tab"
        );
        let visible = visible_mount(&container);
        assert!(
            text_of(&visible).contains("fn alpha()"),
            "the replacing file's contents should render, got {:?}",
            text_of(&visible)
        );
        assert_eq!(
            query_all(&container, ".chat-input-area").len(),
            0,
            "replacing the chat should take its composer with it"
        );
    }

    /// A tab's full label stays available to assistive technology even when
    /// the strip ellipsizes it — dev-docs/32 §11 carries this requirement
    /// into the split tab strips, where duplicate file occurrences are
    /// distinguished by pane name on top of the label.
    #[wasm_bindgen_test]
    async fn full_tab_labels_stay_available_when_visually_ellipsized() {
        let long_label = "a-very-long-file-name-that-the-tab-strip-must-ellipsize.rs";
        let container = make_container();
        let state = AppState::new();
        state.open_tab(TabContent::empty_chat(), long_label.to_owned(), true);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        let tab = tab_button_named(&container, long_label);
        assert_eq!(
            tab.get_attribute("title").as_deref(),
            Some(long_label),
            "the full label should stay available as a tooltip"
        );

        let label = tab
            .query_selector(".tab-label")
            .unwrap()
            .expect("tab label element")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert!(
            label.client_width() as f64 <= TAB_LABEL_MAX_WIDTH,
            "the rendered label should be clamped to the strip's max width, got {}px",
            label.client_width()
        );
        assert!(
            label.scroll_width() > label.client_width(),
            "this label should actually be visually truncated, otherwise the \
             accessible-name guarantee is untested"
        );
        assert_eq!(
            text_of(&label),
            long_label,
            "the truncation should be visual only — the text stays complete"
        );

        let home = tab_button_named(&container, "Home");
        assert_eq!(
            text_of(&home).trim(),
            "",
            "the Home tab renders as an icon with no visible text"
        );
        assert!(
            home.query_selector("[aria-hidden=\"true\"]")
                .unwrap()
                .is_some(),
            "the Home tab's icon should be hidden from assistive technology, \
             leaving its aria-label as the accessible name"
        );
    }

    // ── Split view ──────────────────────────────────────────────────────

    /// Both panes render their own strip and their own active content, and
    /// both active contents stay mounted and visible at once — the core claim
    /// of the split (dev-docs/32 §1, §13).
    #[wasm_bindgen_test]
    async fn split_renders_two_panes_with_both_active_contents_visible() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        assert_eq!(
            panes(&container).len(),
            1,
            "the workspace starts as a single pane"
        );
        assert!(
            divider(&container).is_none(),
            "an unsplit workspace has no divider"
        );

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        assert_eq!(
            panes(&container).len(),
            2,
            "opening into the other pane should produce exactly two panes"
        );
        assert_eq!(
            query_all(&container, "[role=\"tablist\"]").len(),
            2,
            "each pane owns its own tab strip"
        );

        let primary = pane_element(&container, PaneId::Primary);
        let secondary = pane_element(&container, PaneId::Secondary);
        assert!(
            tab_labels_in(&primary).contains(&"alpha.rs".to_owned()),
            "the first file stays in the primary pane"
        );
        assert_eq!(
            tab_labels_in(&secondary),
            vec!["bravo.rs".to_owned()],
            "the second file opens in the secondary pane only"
        );

        let visible = visible_mounts(&container);
        assert_eq!(
            visible.len(),
            2,
            "both panes' active tabs are visible at the same time"
        );
        let rendered: String = visible.iter().map(text_of).collect();
        assert!(
            rendered.contains("fn alpha()") && rendered.contains("fn bravo()"),
            "each pane renders its own file, got {rendered:?}"
        );

        // Both panes' actives are pinned: a tab switch in one pane cannot
        // unmount the other pane's content.
        assert!(
            mount_containing(&container, "fn alpha()").is_some(),
            "the unfocused pane's active tab stays mounted"
        );
        assert!(
            divider(&container).is_some(),
            "a split workspace exposes a divider"
        );
        assert!(
            live_region_text(&container).contains("Split view opened"),
            "opening the split is announced, got {:?}",
            live_region_text(&container)
        );

        // Two occurrences of one file would otherwise be indistinguishable to a
        // screen reader: the pane name is what tells them apart.
        assert_eq!(
            primary.get_attribute("aria-label").as_deref(),
            Some("Editor pane 1 of 2: alpha.rs"),
            "a pane names its position and the tab it is showing"
        );
        assert_eq!(
            secondary.get_attribute("aria-label").as_deref(),
            Some("Editor pane 2 of 2: bravo.rs")
        );
    }

    /// A tab near the right edge — which a half-width split strip makes
    /// ordinary — must not open a menu that hangs off-screen with its items
    /// unreachable.
    #[wasm_bindgen_test]
    async fn the_tab_context_menu_stays_inside_the_viewport() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        let window = web_sys::window().unwrap();
        let view_width = window.inner_width().unwrap().as_f64().unwrap();
        let view_height = window.inner_height().unwrap().as_f64().unwrap();

        // Open the menu hard against the bottom-right corner.
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_client_x(view_width as i32 - 2);
        init.set_client_y(view_height as i32 - 2);
        let event =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &init).unwrap();
        tab_button_named(&container, "Chat")
            .dispatch_event(&event)
            .unwrap();
        settle().await;

        let menu = query(&container, ".context-menu").expect("the context menu opened");
        let rect = menu.get_bounding_client_rect();
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "precondition: the menu has a real box"
        );
        assert!(
            rect.right() <= view_width,
            "the menu must not overflow the right edge: right {} vs viewport {view_width}",
            rect.right()
        );
        assert!(
            rect.bottom() <= view_height,
            "the menu must not overflow the bottom edge: bottom {} vs viewport {view_height}",
            rect.bottom()
        );
        assert!(
            rect.left() >= 0.0 && rect.top() >= 0.0,
            "clamping must not push the menu off the opposite edge"
        );
    }

    /// Pane widths follow the split ratio, and the 320px pane minimum wins over
    /// an extreme ratio rather than producing an unusable sliver.
    #[wasm_bindgen_test]
    async fn split_geometry_follows_the_ratio_and_respects_the_pane_minimum() {
        let width = 1200.0;
        let container = make_sized_container(width, CONTAINER_HEIGHT);
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        let primary = pane_element(&container, PaneId::Primary);
        let secondary = pane_element(&container, PaneId::Secondary);
        let primary_width = primary.get_bounding_client_rect().width();
        let secondary_width = secondary.get_bounding_client_rect().width();

        assert!(
            (primary_width - secondary_width).abs() <= PANE_DIVIDER_WIDTH + 1.0,
            "a new split is 50/50: primary {primary_width}px vs secondary {secondary_width}px"
        );
        assert!(
            (primary_width + secondary_width + PANE_DIVIDER_WIDTH - width).abs() < 2.0,
            "the two panes plus the divider fill the workspace"
        );

        state.set_split_ratio(SplitRatio::new(0.7));
        settle().await;

        let primary_width = pane_element(&container, PaneId::Primary)
            .get_bounding_client_rect()
            .width();
        let secondary_width = pane_element(&container, PaneId::Secondary)
            .get_bounding_client_rect()
            .width();
        assert!(
            (primary_width - width * 0.7).abs() < 6.0,
            "a 70% ratio should give the primary pane ~{}px, got {primary_width}px",
            width * 0.7
        );
        assert!(
            secondary_width >= MIN_PANE_WIDTH,
            "the secondary pane must never fall below its {MIN_PANE_WIDTH}px minimum, \
             got {secondary_width}px"
        );

        // SplitRatio clamps beyond 80% — the pane cannot be starved even by a
        // programmatic value.
        state.set_split_ratio(SplitRatio::new(0.99));
        settle().await;
        let secondary_width = pane_element(&container, PaneId::Secondary)
            .get_bounding_client_rect()
            .width();
        assert!(
            secondary_width >= MIN_PANE_WIDTH,
            "an out-of-range ratio is clamped, leaving the secondary pane usable, \
             got {secondary_width}px"
        );
    }

    /// The divider is a real separator: pointer drag, arrow keys, Shift steps,
    /// Home/End, double-click reset, and live ARIA values (dev-docs/32 §11).
    #[wasm_bindgen_test]
    async fn divider_resizes_by_pointer_and_keyboard_and_exposes_aria() {
        let width = 1200.0;
        let container = make_sized_container(width, CONTAINER_HEIGHT);
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        let separator = divider(&container).expect("split exposes a separator");
        assert_eq!(
            separator.get_attribute("aria-orientation").as_deref(),
            Some("vertical")
        );
        // 320px minima over 1195px of usable width: the primary pane physically
        // cannot go below 27% or above 73% here, so those are the bounds the
        // separator advertises. (It used to claim 20/80 — positions the pane
        // could not reach; QA F1.)
        assert_eq!(
            separator.get_attribute("aria-valuemin").as_deref(),
            Some("27"),
            "the separator advertises the bound this width can actually reach"
        );
        assert_eq!(
            separator.get_attribute("aria-valuemax").as_deref(),
            Some("73"),
            "and the upper bound it can actually reach"
        );
        assert_eq!(
            separator.get_attribute("aria-valuenow").as_deref(),
            Some("50"),
            "a new split starts at 50%"
        );
        assert!(
            separator
                .get_attribute("aria-label")
                .is_some_and(|label| !label.is_empty()),
            "the separator has an accessible name"
        );

        // Pointer drag to 60% of the workspace.
        let panes_rect = query(&container, ".center-panes")
            .expect("pane row")
            .get_bounding_client_rect();
        let target_x = (panes_rect.left() + panes_rect.width() * 0.6) as i32;
        separator
            .dispatch_event(&pointer_event("pointerdown", target_x))
            .unwrap();
        separator
            .dispatch_event(&pointer_event("pointermove", target_x))
            .unwrap();
        separator
            .dispatch_event(&pointer_event("pointerup", target_x))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("60"),
            "dragging the divider to 60% of the workspace sets a 60% ratio"
        );

        // Arrow keys: 2% normally, 10% with Shift.
        let separator = divider(&container).unwrap();
        separator
            .dispatch_event(&key_event("ArrowRight", false))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("62"),
            "Right Arrow widens the primary pane by 2%"
        );

        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("ArrowLeft", true))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("52"),
            "Shift+Left narrows the primary pane by 10%"
        );

        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("End", false))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("73"),
            "End selects the upper bound this width can reach"
        );

        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("Home", false))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("27"),
            "Home selects the lower bound this width can reach"
        );
        assert!(
            live_region_text(&container).contains("27 percent"),
            "resizes are announced politely, got {:?}",
            live_region_text(&container)
        );
        // A bare number on a separator reads as "27" with no unit. `aria-valuetext`
        // is what a screen reader speaks instead.
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuetext")
                .as_deref(),
            Some("27 percent"),
            "the separator speaks a unit, not a bare number"
        );

        let dblclick = web_sys::MouseEvent::new("dblclick").unwrap();
        divider(&container)
            .unwrap()
            .dispatch_event(&dblclick)
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("50"),
            "double-click restores 50/50"
        );
    }

    /// A 5px line is a cruel pointer target. The divider stays 5px of ink but
    /// is grabbable well beyond it.
    #[wasm_bindgen_test]
    async fn the_divider_is_grabbable_beyond_its_visible_line() {
        let container = make_sized_container(1200.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        let separator = divider(&container).expect("split exposes a separator");
        let rect = separator.get_bounding_client_rect();
        assert!(
            rect.width() <= 6.0,
            "the visible line stays thin: {}px",
            rect.width()
        );

        // Hit-test either side of the line, out to the edge of the 24px minimum
        // grab band. A pseudo-element hit resolves to its originating element,
        // so the divider must answer for both points.
        let document = web_sys::window().unwrap().document().unwrap();
        let middle = rect.top() + rect.height() / 2.0;
        let reach = (MIN_TARGET_SIZE - rect.width()) / 2.0;
        assert!(reach >= 9.0, "the band must extend at least 9px each side");
        for offset in [-reach, rect.width() + reach] {
            let hit = document
                .element_from_point((rect.left() + offset) as f32, middle as f32)
                .expect("something is under the pointer");
            assert!(
                hit.is_same_node(Some(&separator)),
                "the divider must be grabbable {offset}px from its edge, but the \
                 point resolved to {:?}",
                hit.class_name()
            );
        }
    }

    /// Shortcut text is never hand-written: it is rendered from the binding that
    /// fires, so a macOS user is not told to press a chord their keyboard does
    /// not have.
    #[wasm_bindgen_test]
    async fn shortcut_text_is_rendered_from_the_bindings_not_hardcoded() {
        let container = make_sized_container(500.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        let expected = binding_for(ActionId::Command(CommandId::FocusPrimaryPane))
            .expect("Focus Primary Pane is bound")
            .chord()
            .hint();
        let notice = query(&container, ".center-narrow-notice").expect("narrow notice");
        assert!(
            text_of(&notice).contains(&expected),
            "the narrow-mode switch advertises the chord that actually fires \
             ({expected}), got {:?}",
            text_of(&notice)
        );
    }

    /// Two menus can be open at once (a pane menu and a tab menu), and both may
    /// list the same command. Reason ids derived from the label would collide,
    /// and `aria-describedby` would resolve to whichever came first.
    #[wasm_bindgen_test]
    async fn menu_reason_ids_are_unique_even_with_two_menus_open() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // A single pane: the pane menu's Close Editor Pane / Return to Single
        // Pane commands are both unavailable, so each renders its reason element.
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        query(&container, ".pane-actions-trigger")
            .expect("pane actions trigger")
            .click();
        settle().await;
        tab_button_named(&container, "Chat")
            .dispatch_event(&web_sys::MouseEvent::new("contextmenu").unwrap())
            .unwrap();
        settle().await;

        let described: Vec<String> = query_all(&container, "[aria-describedby]")
            .iter()
            .filter_map(|element| element.get_attribute("aria-describedby"))
            .collect();
        assert!(
            described.len() >= 2,
            "precondition: two menus are open, each describing an unavailable \
             command, got {described:?}"
        );

        let document = web_sys::window().unwrap().document().unwrap();
        for id in &described {
            let matches = document
                .query_selector_all(&format!("[id=\"{id}\"]"))
                .unwrap()
                .length();
            assert_eq!(
                matches, 1,
                "aria-describedby={id:?} must resolve to exactly one element, \
                 found {matches}"
            );
        }
        let mut unique = described.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            described.len(),
            "menu reason ids must be unique by construction, got {described:?}"
        );
    }

    /// The tab-strip controls need a real target, and the commands they carry
    /// also reach a full-size (>=44px) target in the menus.
    #[wasm_bindgen_test]
    async fn strip_controls_and_menu_items_meet_their_target_sizes() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        for selector in [".pane-actions-trigger"] {
            let control = query(&container, selector).unwrap_or_else(|| panic!("{selector}"));
            let rect = control.get_bounding_client_rect();
            assert!(
                rect.width() >= 24.0 && rect.height() >= 24.0,
                "{selector} must be at least 24x24, got {}x{}",
                rect.width(),
                rect.height()
            );
        }

        query(&container, ".pane-actions-trigger")
            .expect("pane actions trigger")
            .click();
        settle().await;
        let items = query_all(&container, ".pane-actions-menu [role=\"menuitem\"]");
        assert!(!items.is_empty(), "the pane menu lists its commands");
        for item in &items {
            let rect = item.get_bounding_client_rect();
            assert!(
                rect.height() >= 44.0,
                "menu items are the full-size target for these commands: expected \
                 >=44px tall, got {}px",
                rect.height()
            );
        }
    }

    /// Fixtures do not inherit the previous test's page or its presentation
    /// state.
    ///
    /// Every test in a wasm binary shares one document and one set of
    /// presentation globals (the workspace measurement, the live-region
    /// message). A container left behind keeps a fixed, full-viewport box in the
    /// page — which is how a divider hit-test ended up resolving to another
    /// fixture's file view — and a stale announcement would let a later test
    /// pass on a message it never produced.
    #[wasm_bindgen_test]
    async fn each_fixture_owns_the_page_and_presentation_state_it_asserts_about() {
        let previous = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let handle = mount_to(previous.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;
        announce("a message from the previous test");
        settle().await;
        assert!(
            !live_region_text(&previous).is_empty(),
            "precondition: the previous fixture announced something"
        );
        assert!(
            workspace_width().get_untracked().is_some(),
            "precondition: the previous fixture measured its workspace"
        );
        drop(handle);
        settle().await;

        // A new fixture disposes the old page ...
        let container = make_container();
        let document = web_sys::window().unwrap().document().unwrap();
        assert_eq!(
            document
                .query_selector_all("[data-test-container]")
                .unwrap()
                .length(),
            1,
            "exactly one test container is in the page: the current one"
        );
        assert!(
            !previous.is_connected(),
            "the previous fixture's DOM is gone, not merely hidden behind the new \
             one — a leftover fixed container shadows hit-testing and hover"
        );

        // ... and starts from clean presentation state.
        assert_eq!(
            workspace_width().get_untracked(),
            None,
            "the new fixture does not inherit the previous workspace's measurement"
        );

        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;
        assert_eq!(
            live_region_text(&container).trim(),
            "",
            "and the live region starts silent: a test must not be able to pass \
             on an announcement the previous one made"
        );
    }

    /// The width outlives the owner that measured it.
    ///
    /// It used to be an `RwSignal` created by whichever surface asked first and
    /// then held by globals (the `ResizeObserver` closure, the window keydown
    /// listener) — so disposing that owner left every other reader holding a
    /// dead handle, and the next suite to touch it panicked with *"you tried to
    /// access a reactive value ... already disposed"*. This mounts, disposes,
    /// mounts again, and uses the width across the boundary.
    #[wasm_bindgen_test]
    async fn the_workspace_width_survives_owner_disposal_and_remount() {
        // First workspace: narrow, so it leaves a measurement worth not leaking.
        let container = make_sized_container(500.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;
        assert_eq!(
            workspace_width().get_untracked(),
            Some(500.0),
            "the mounted workspace measured itself"
        );

        // Dispose the owner that did the measuring.
        drop(handle);
        settle().await;

        // Reading through the shared handle must still work — no dead signal —
        // and the measurement must be gone, not stale: that workspace no longer
        // exists, so a narrow number must not disable split for what comes next.
        assert_eq!(
            workspace_width().get_untracked(),
            None,
            "a torn-down workspace has no width, and must not leave a narrow one \
             behind to disable split for the next surface"
        );

        // Mount a second, wider workspace and use the width through the shared
        // 645px gate — the path that panicked for 51 tests.
        let container = make_sized_container(1200.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        let width = workspace_width();
        assert_eq!(
            width.get_untracked(),
            Some(1200.0),
            "the remounted workspace measures itself through the same handle"
        );

        // Drive a split, then use narrow mode — the one width-reactive surface
        // that remains — to prove the shared width signal is read live and its
        // changes still propagate across the remount boundary.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;
        assert!(
            query(&container, ".center-narrow-notice").is_none(),
            "a 1200px workspace is wide enough for two panes, so no narrow notice"
        );

        // Shrink the real container rather than poking the signal. The live
        // `ResizeObserver` re-measures the actual panes row on every
        // (re-)observe, so a faked `width.set(400)` on a genuinely 1200px
        // container is correctly overwritten back to 1200 during settling —
        // the component keeping the signal truthful is the desired behavior,
        // and asserting on a forced lie rejected it. Driving a real resize
        // proves the same contract end-to-end: the shared handle is live
        // across the remount boundary AND the observer that feeds it works.
        container
            .style()
            .set_property("width", "400px")
            .expect("shrink test container");
        settle().await;
        assert_eq!(
            width.get_untracked(),
            Some(400.0),
            "the real resize reaches the shared width handle across the \
             remount boundary"
        );
        assert!(
            query(&container, ".center-narrow-notice").is_some(),
            "a width change still propagates reactively — the signal is \
             reference-counted, not dead"
        );
    }

    /// Split a container and settle, returning nothing — the caller reads the
    /// DOM. Used by the geometry tests, which all need the same two-pane setup.
    async fn split_two_files(state: &AppState) {
        open_file_tab(state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;
    }

    fn pane_widths(container: &HtmlElement) -> (f64, f64) {
        (
            pane_element(container, PaneId::Primary)
                .get_bounding_client_rect()
                .width(),
            pane_element(container, PaneId::Secondary)
                .get_bounding_client_rect()
                .width(),
        )
    }

    /// **QA F1.** At 911px the 320px minima physically bound the primary pane to
    /// 35–65%. The separator used to advertise 20–80 and announce positions the
    /// pane could not reach — a screen-reader user was told "20 percent" while
    /// the pane sat at 35, and the keyboard had dead zones at both ends.
    ///
    /// Everything the separator reports must now be what the panes actually do.
    #[wasm_bindgen_test]
    async fn at_a_clamped_width_the_divider_reports_what_the_panes_can_reach() {
        let container = make_sized_container(911.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;
        split_two_files(&state).await;

        // 906px of usable width (911 less the 5px divider); a 320px minimum is
        // 35.3% of it, so the reachable range is 35–65.
        let separator = divider(&container).expect("separator");
        assert_eq!(
            separator.get_attribute("aria-valuemin").as_deref(),
            Some("35"),
            "the lower bound is the one 320px imposes at this width, not 20"
        );
        assert_eq!(
            separator.get_attribute("aria-valuemax").as_deref(),
            Some("65"),
            "and the upper bound is 65, not 80"
        );

        // Home goes to the real minimum, and the pane is really there.
        separator.dispatch_event(&key_event("Home", false)).unwrap();
        settle().await;
        let separator = divider(&container).unwrap();
        assert_eq!(
            separator.get_attribute("aria-valuenow").as_deref(),
            Some("35"),
            "Home reports the position the pane actually took"
        );
        assert_eq!(
            separator.get_attribute("aria-valuetext").as_deref(),
            Some("35 percent")
        );
        let (primary, secondary) = pane_widths(&container);
        assert!(
            (primary - MIN_PANE_WIDTH).abs() <= 1.0,
            "the primary pane is at its 320px minimum, got {primary}px"
        );
        assert!(
            (secondary - (911.0 - PANE_DIVIDER_WIDTH - MIN_PANE_WIDTH)).abs() <= 1.0,
            "and the secondary pane has the rest, got {secondary}px"
        );

        // Pressing further does nothing — so it must say nothing. A separator
        // that announces a move it did not make is worse than silence.
        announce("sentinel");
        settle().await;
        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("ArrowLeft", false))
            .unwrap();
        settle().await;
        assert_eq!(
            live_region_text(&container).trim(),
            "sentinel",
            "a keypress that cannot move the divider must not announce a change"
        );
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("35"),
            "and the reported position is unchanged, because the panes are"
        );
        let (still_primary, _) = pane_widths(&container);
        assert!(
            (still_primary - primary).abs() <= 0.5,
            "the pane really did not move"
        );

        // The same at the other end.
        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("End", false))
            .unwrap();
        settle().await;
        assert_eq!(
            divider(&container)
                .unwrap()
                .get_attribute("aria-valuenow")
                .as_deref(),
            Some("65")
        );
        let (primary, secondary) = pane_widths(&container);
        assert!(
            (secondary - MIN_PANE_WIDTH).abs() <= 1.0,
            "the secondary pane is at its minimum, got {secondary}px"
        );
        announce("sentinel");
        settle().await;
        divider(&container)
            .unwrap()
            .dispatch_event(&key_event("ArrowRight", false))
            .unwrap();
        settle().await;
        assert_eq!(
            live_region_text(&container).trim(),
            "sentinel",
            "nor at the upper bound"
        );
        let (still_primary, _) = pane_widths(&container);
        assert!((still_primary - primary).abs() <= 0.5);
    }

    /// **QA F2.** "50/50" measured 455.5 / 450.5 — the 5px divider was charged
    /// entirely to the secondary pane. The ratio is now a share of the *usable*
    /// width, so equal means equal.
    #[wasm_bindgen_test]
    async fn fifty_fifty_gives_both_panes_the_same_width() {
        for workspace in [911.0, 1200.0] {
            let container = make_sized_container(workspace, CONTAINER_HEIGHT);
            let state = AppState::new();
            let state_for_mount = state.clone();
            let _handle = mount_to(container.clone(), move || {
                provide_context(state_for_mount.clone());
                install_tab_lru_effect(&state_for_mount);
                view! { <CenterZone /> }
            });
            settle().await;
            split_two_files(&state).await;

            let separator = divider(&container).expect("separator");
            separator
                .dispatch_event(&web_sys::MouseEvent::new("dblclick").unwrap())
                .unwrap();
            settle().await;

            assert_eq!(
                divider(&container)
                    .unwrap()
                    .get_attribute("aria-valuenow")
                    .as_deref(),
                Some("50"),
                "{workspace}px: double-click restores an even split"
            );
            let (primary, secondary) = pane_widths(&container);
            assert!(
                (primary - secondary).abs() <= 1.0,
                "{workspace}px: equal panes must be equal within 1px, got \
                 {primary} vs {secondary}"
            );
            assert!(
                (primary + secondary + PANE_DIVIDER_WIDTH - workspace).abs() <= 1.0,
                "{workspace}px: and together with the divider they fill the workspace"
            );
        }
    }

    /// **QA F4.** Keyboard steps accumulated binary noise and persisted it —
    /// `0.30000000000000004` reached local storage. Every ratio the divider
    /// produces is now exact at the precision a layout can use.
    #[wasm_bindgen_test]
    async fn keyboard_steps_produce_exact_ratios_and_wide_workspaces_use_the_policy_bounds() {
        // 1795px of usable width: 320px is under 20% of it, so here the policy
        // bound (20–80%) is the binding one — the physical minimum is not.
        let container = make_sized_container(1800.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;
        split_two_files(&state).await;

        let separator = divider(&container).expect("separator");
        assert_eq!(
            separator.get_attribute("aria-valuemin").as_deref(),
            Some("20"),
            "a wide workspace can reach the policy bound, so that is what it reports"
        );
        assert_eq!(
            separator.get_attribute("aria-valuemax").as_deref(),
            Some("80")
        );

        // 0.5 → 0.48 → 0.46, and each stored value is exact.
        for expected in ["48", "46"] {
            divider(&container)
                .unwrap()
                .dispatch_event(&key_event("ArrowLeft", false))
                .unwrap();
            settle().await;
            assert_eq!(
                divider(&container)
                    .unwrap()
                    .get_attribute("aria-valuenow")
                    .as_deref(),
                Some(expected)
            );
        }

        let stored = state.center_split_ratio.get_untracked().get();
        assert_eq!(
            stored,
            (stored * 10_000.0).round() / 10_000.0,
            "the ratio that gets persisted carries no float noise, got {stored}"
        );
        assert!(
            !format!("{stored}").contains("000000"),
            "and it serializes cleanly, got {stored}"
        );
        assert!(
            (stored - 0.46).abs() < 1e-9,
            "two 2% steps down from an even split land exactly on 0.46, got {stored}"
        );
    }

    /// A workspace too narrow for two panes keeps both panes in state and in
    /// the DOM, shows only the focused one, explains itself, and offers a way
    /// back (dev-docs/32 §11).
    #[wasm_bindgen_test]
    async fn narrow_workspace_hides_the_unfocused_pane_but_keeps_it_mounted() {
        // Below MIN_SPLIT_WIDTH (645px), so the real width observer — not a
        // stubbed value — drives narrow mode.
        let container = make_sized_container(500.0, CONTAINER_HEIGHT);
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;

        assert_eq!(
            panes(&container).len(),
            2,
            "narrow mode must not destroy the split: both panes stay in the DOM"
        );
        assert!(
            divider(&container).is_none(),
            "there is nothing to drag while only one pane is shown"
        );
        let visible = visible_mounts(&container);
        assert_eq!(
            visible.len(),
            1,
            "only the focused pane's content is visible while narrow"
        );
        assert!(
            text_of(&visible[0]).contains("fn bravo()"),
            "the focused (secondary) pane is the one shown"
        );

        let notice = query(&container, ".center-narrow-notice")
            .expect("narrow mode explains itself with a visible notice");
        assert!(
            text_of(&notice).contains("too narrow"),
            "the notice states why only one pane is shown, got {:?}",
            text_of(&notice)
        );

        // The pane-switch control is part of the notice, so a user can reach
        // the hidden pane without a keyboard shortcut.
        let show_primary = query_all(&notice, "button")
            .into_iter()
            .find(|button| text_of(button).contains("Primary"))
            .expect("the notice offers a way back to the other pane");
        show_primary.click();
        settle().await;

        let visible = visible_mounts(&container);
        assert_eq!(visible.len(), 1, "still one visible pane while narrow");
        assert!(
            text_of(&visible[0]).contains("fn alpha()"),
            "switching panes in narrow mode shows the other pane's content"
        );
        assert!(
            mount_containing(&container, "fn bravo()").is_some(),
            "the now-hidden pane keeps its content mounted — nothing is discarded"
        );
    }

    /// dev-docs/32 §7: the composer belongs to the composer owner, not to the
    /// focused pane. A chat beside a focused file keeps exactly one composer.
    #[wasm_bindgen_test]
    async fn composer_stays_with_the_chat_while_the_file_pane_is_focused() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        // A *live* chat: `ToolOutputModeToggle` renders in the chat's agent
        // header, so only a chat with an agent can carry it. A draft has no
        // header, and asserting "exactly one toggle" against one asserted
        // nothing.
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "test-host".to_owned(),
                agent_id: protocol::AgentId("beside".to_owned()),
            }),
            "Chat".to_owned(),
            true,
        );
        settle().await;

        assert_eq!(
            query_all(&container, ".chat-input-area").len(),
            1,
            "one chat in the split means exactly one composer"
        );

        // Focus the file pane. The chat is now in the *unfocused* pane — and
        // must keep the composer, because it is still the only chat.
        let primary = pane_element(&container, PaneId::Primary);
        tab_button_named_in(&primary, "alpha.rs").click();
        settle().await;

        let composers = query_all(&container, ".chat-input-area");
        assert_eq!(
            composers.len(),
            1,
            "focusing the file pane must not take the composer away from the chat"
        );
        let secondary = pane_element(&container, PaneId::Secondary);
        assert!(
            secondary.contains(Some(&composers[0])),
            "the composer stays in the chat's pane, not the focused file pane"
        );
        assert!(
            text_of(&pane_element(&container, PaneId::Primary)).contains("fn alpha()"),
            "the focused pane still shows its own file"
        );

        // Exactly one client-global tool-output toggle, rendered with the
        // composer owner (dev-docs/32 §7) — never one per pane, never per agent.
        assert_eq!(
            query_all(&container, ".tool-output-mode-toggle").len(),
            1,
            "the tool-output preference is client-global: exactly one control, \
             rendered with the composer owner"
        );

        // A second chat in the focused pane takes ownership; still one composer.
        state.open_tab_in(
            PaneId::Primary,
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "test-host".to_owned(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
            }),
            "Second Chat".to_owned(),
            true,
        );
        settle().await;
        let composers = query_all(&container, ".chat-input-area");
        assert_eq!(
            composers.len(),
            1,
            "two chats in a split still mount exactly one composer"
        );
        assert!(
            pane_element(&container, PaneId::Primary).contains(Some(&composers[0])),
            "with a chat in each pane the focused pane owns the composer"
        );
        assert_eq!(
            query_all(&container, ".tool-output-mode-toggle").len(),
            1,
            "the tool-output toggle does not duplicate in a two-chat split"
        );
    }

    /// Splits and cross-pane moves are created by dragging tabs — never by a
    /// button, menu item, or palette command. With a loaded file (which used to
    /// enable Split Right), no split/move control renders anywhere: not on the
    /// tab strip, not in the pane-actions menu, not in the tab context menu.
    #[wasm_bindgen_test]
    async fn no_split_or_move_controls_are_rendered() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // A loaded file is the exact state that used to enable the split control.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;

        assert!(
            query(&container, ".split-right-btn").is_none(),
            "the tab-strip split control is gone — splits are created by dragging"
        );

        // The pane-actions menu lists only pane focus/close commands now.
        query(&container, ".pane-actions-trigger")
            .expect("pane actions trigger")
            .click();
        settle().await;
        let pane_items = query_all(&container, ".pane-actions-menu [role=\"menuitem\"]");
        assert!(
            !pane_items.is_empty(),
            "the pane menu still lists its close/join commands"
        );
        for item in &pane_items {
            let label = text_of(item);
            assert!(
                !label.contains("Split Right") && !label.contains("Move Tab to Other Pane"),
                "no split/move command remains in the pane menu, found {label:?}"
            );
        }

        // The tab context menu carries no split/move item either.
        tab_button_named(&container, "alpha.rs")
            .dispatch_event(&web_sys::MouseEvent::new("contextmenu").unwrap())
            .unwrap();
        settle().await;
        for item in query_all(&container, ".context-menu [role=\"menuitem\"]") {
            let label = text_of(&item);
            assert!(
                !label.contains("Split Right") && !label.contains("Move Tab to Other Pane"),
                "no split/move command remains in the tab menu, found {label:?}"
            );
        }
    }

    /// Cross-pane drag is move-only, refuses its own pane, and collapses the
    /// split when the last tab leaves a pane (dev-docs/32 §10).
    #[wasm_bindgen_test]
    async fn cross_pane_drag_moves_a_tab_and_collapses_the_split() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::empty_chat(),
            "Chat".to_owned(),
            true,
        );
        settle().await;

        let secondary = pane_element(&container, PaneId::Secondary);
        let chat_tab = tab_button_named_in(&secondary, "Chat");
        assert_eq!(
            chat_tab.get_attribute("draggable").as_deref(),
            Some("true"),
            "tabs become draggable once a split exists"
        );

        chat_tab.dispatch_event(&drag_event("dragstart")).unwrap();
        settle().await;

        // Dropping on the source pane is refused.
        secondary.dispatch_event(&drag_event("dragover")).unwrap();
        settle().await;
        assert!(
            query(&container, ".pane-drop-overlay").is_none(),
            "the source pane is never a valid drop target"
        );

        let primary = pane_element(&container, PaneId::Primary);
        primary.dispatch_event(&drag_event("dragover")).unwrap();
        settle().await;
        assert!(
            query(&container, ".pane-drop-overlay").is_some(),
            "the other pane shows a drop target overlay"
        );

        primary.dispatch_event(&drag_event("drop")).unwrap();
        settle().await;

        assert_eq!(
            panes(&container).len(),
            1,
            "moving the last tab out of a pane collapses the split"
        );
        let survivor = pane_element(&container, PaneId::Primary);
        let labels = tab_labels_in(&survivor);
        assert!(
            labels.contains(&"Chat".to_owned()) && labels.contains(&"alpha.rs".to_owned()),
            "the moved tab lands in the target pane beside what was already there, got {labels:?}"
        );
        assert!(
            query(&container, ".pane-drop-overlay").is_none(),
            "drag state is cleaned up after the drop"
        );
        assert!(
            live_region_text(&container).contains("Split view closed"),
            "collapsing back to one pane is announced, got {:?}",
            live_region_text(&container)
        );
    }

    /// Tab strips carry real tablist/tab/tabpanel semantics with roving focus
    /// (dev-docs/32 §11).
    #[wasm_bindgen_test]
    async fn tab_strips_expose_tablist_tab_and_tabpanel_semantics() {
        let container = make_container();
        let state = AppState::new();

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;

        let tablist = query(&container, "[role=\"tablist\"]").expect("the strip is a tablist");
        assert!(
            tablist
                .get_attribute("aria-label")
                .is_some_and(|label| !label.is_empty()),
            "the tablist has an accessible name"
        );

        let chat_tab = tab_button_named(&container, "Chat");
        let file_tab = tab_button_named(&container, "alpha.rs");
        assert_eq!(chat_tab.get_attribute("role").as_deref(), Some("tab"));
        assert_eq!(
            chat_tab.get_attribute("aria-selected").as_deref(),
            Some("true"),
            "the active tab is the selected one"
        );
        assert_eq!(
            file_tab.get_attribute("aria-selected").as_deref(),
            Some("false"),
            "an inactive tab is not selected"
        );
        assert_eq!(
            chat_tab.get_attribute("tabindex").as_deref(),
            Some("0"),
            "roving focus: only the active tab is in the tab order"
        );
        assert_eq!(
            file_tab.get_attribute("tabindex").as_deref(),
            Some("-1"),
            "roving focus: inactive tabs are skipped by Tab"
        );

        // Each tab points at the panel it controls, and the panel points back.
        let panel_id = chat_tab
            .get_attribute("aria-controls")
            .expect("a tab controls its panel");
        let panel = query(&container, &format!("#{panel_id}")).expect("the panel exists");
        assert_eq!(
            panel.get_attribute("role").as_deref(),
            Some("tabpanel"),
            "tab content is a tabpanel"
        );
        assert_eq!(
            panel.get_attribute("aria-labelledby").as_deref(),
            chat_tab.get_attribute("id").as_deref(),
            "the panel is named by its tab"
        );

        // Arrow keys move between tabs within the strip.
        chat_tab
            .dispatch_event(&key_event("ArrowLeft", false))
            .unwrap();
        settle().await;
        assert_eq!(
            tab_button_named(&container, "alpha.rs")
                .get_attribute("aria-selected")
                .as_deref(),
            Some("true"),
            "Left Arrow moves selection to the previous tab in the strip"
        );
    }

    // ── Command and menu policy ─────────────────────────────────────────

    fn go_to_chat(state: &AppState) {
        crate::components::command_palette::execute_command(state, CommandId::GoToChat, None);
    }

    fn active_tab_is_chat(state: &AppState) -> bool {
        state.center_zone.with_untracked(|center_zone| {
            center_zone
                .active_tab()
                .is_some_and(|tab| matches!(tab.content, TabContent::Chat { .. }))
        })
    }

    fn tab_count(state: &AppState) -> usize {
        state
            .center_zone
            .with_untracked(|center_zone| center_zone.all_tab_ids().len())
    }

    /// "Go to Chat" takes you to the chat you would be typing into. Priority:
    /// the composer owner, then a chat hidden in the focused pane, then a chat
    /// in the other pane, and only then a new draft.
    #[wasm_bindgen_test]
    async fn go_to_chat_prefers_the_composer_owner() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // A chat in the secondary pane owns the composer while a file in the
        // focused primary pane has focus.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::empty_chat(),
            "Chat".to_owned(),
            true,
        );
        settle().await;
        let chat = state
            .center_zone
            .with_untracked(|center_zone| center_zone.pane_active_tab_id(PaneId::Secondary))
            .expect("chat is the secondary pane's active tab");
        let primary_file = pane_element(&container, PaneId::Primary);
        tab_button_named_in(&primary_file, "alpha.rs").click();
        settle().await;

        let before = tab_count(&state);
        go_to_chat(&state);
        settle().await;

        assert_eq!(
            tab_count(&state),
            before,
            "an existing chat must be revealed, never duplicated by a new draft"
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id()),
            Some(chat),
            "Go to Chat reveals the composer owner, even in the other pane"
        );
        assert!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.focused_id())
                == PaneId::Secondary,
            "revealing a tab focuses the pane that holds it"
        );
    }

    #[wasm_bindgen_test]
    async fn go_to_chat_prefers_a_hidden_focused_pane_chat_over_the_other_pane() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // Primary: a hidden chat behind an active file. Secondary: another
        // hidden chat behind its own active file. Neither owns the composer.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Focused Chat".to_owned(), true);
        settle().await;
        open_file_tab_in(&state, PaneId::Secondary, "bravo.rs", "fn bravo() {}");
        settle().await;
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "test-host".to_owned(),
                agent_id: protocol::AgentId("other".to_owned()),
            }),
            "Other Chat".to_owned(),
            true,
        );
        settle().await;

        // Make each pane's active tab a file, so no chat owns the composer.
        let secondary = pane_element(&container, PaneId::Secondary);
        tab_button_named_in(&secondary, "bravo.rs").click();
        settle().await;
        let primary = pane_element(&container, PaneId::Primary);
        tab_button_named_in(&primary, "alpha.rs").click();
        settle().await;
        assert!(
            !active_tab_is_chat(&state),
            "precondition: no chat owns the composer"
        );

        let before = tab_count(&state);
        go_to_chat(&state);
        settle().await;

        assert_eq!(tab_count(&state), before, "no draft is created");
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.focused_id()),
            PaneId::Primary,
            "a chat hidden in the focused pane wins over a chat in the other pane"
        );
        let revealed = state.center_zone.with_untracked(|center_zone| {
            center_zone
                .active_tab()
                .map(|tab| tab.label.clone())
                .unwrap_or_default()
        });
        assert_eq!(revealed, "Focused Chat");
    }

    #[wasm_bindgen_test]
    async fn go_to_chat_creates_exactly_one_draft_when_no_chat_exists() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        let before = tab_count(&state);

        go_to_chat(&state);
        settle().await;
        assert_eq!(tab_count(&state), before + 1, "one draft is created");
        assert!(active_tab_is_chat(&state), "and it is revealed");

        // Running it again finds that draft rather than opening a second one.
        go_to_chat(&state);
        settle().await;
        assert_eq!(
            tab_count(&state),
            before + 1,
            "Go to Chat never accumulates drafts"
        );
    }

    /// An unavailable menu item is not inert: it keeps its place, stays
    /// keyboard reachable, describes its reason, refuses to act, and announces
    /// that reason.
    #[wasm_bindgen_test]
    async fn an_unavailable_menu_item_stays_reachable_refuses_and_says_why() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // A single pane: "Return to Single Pane" cannot run and must say so
        // rather than vanish from the pane-actions menu.
        state.open_tab(TabContent::empty_chat(), "Chat".to_owned(), true);
        settle().await;
        query(&container, ".pane-actions-trigger")
            .expect("pane actions trigger")
            .click();
        settle().await;

        let item = query_all(&container, ".pane-actions-menu button[role=\"menuitem\"]")
            .into_iter()
            .find(|button| text_of(button).contains("Return to Single Pane"))
            .expect("the join command stays listed even when it cannot run");

        assert_eq!(
            item.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "unavailable items are aria-disabled, not removed"
        );
        assert!(
            !item.has_attribute("disabled"),
            "the item must stay focusable — a control you cannot reach cannot \
             tell you why it is unavailable"
        );
        let described_by = item
            .get_attribute("aria-describedby")
            .expect("an unavailable item is described by its reason");
        let description =
            query(&container, &format!("#{described_by}")).expect("the description element exists");
        assert!(
            text_of(&description).contains("There is only one pane."),
            "the description is the specific reason, got {:?}",
            text_of(&description)
        );

        item.click();
        settle().await;
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "activating an unavailable item performs no work"
        );
        assert!(
            live_region_text(&container).contains("There is only one pane."),
            "the refusal is announced, got {:?}",
            live_region_text(&container)
        );
    }

    /// Dragging a tab onto a pane that already holds the same resource is
    /// refused outright: no accepting overlay, no drop, the reason announced
    /// once, and the occurrence already there highlighted.
    #[wasm_bindgen_test]
    async fn dragging_onto_a_pane_that_already_holds_the_resource_is_refused() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // The same file in both panes: the only resource allowed two occurrences.
        // Placed via the state-layer duplicate primitive — the split creation UI
        // is gone, but the underlying capability that tab-dragging builds on
        // remains, and here it just sets up the precondition.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        let file_tab = state
            .center_zone
            .with_untracked(|center_zone| center_zone.active_tab_id())
            .expect("the file tab is active");
        state.duplicate_file_in_result(PaneId::Secondary, file_tab);
        settle().await;
        assert_eq!(panes(&container).len(), 2, "precondition: a split exists");

        let primary = pane_element(&container, PaneId::Primary);
        let secondary = pane_element(&container, PaneId::Secondary);
        tab_button_named_in(&primary, "alpha.rs")
            .dispatch_event(&drag_event("dragstart"))
            .unwrap();
        settle().await;

        secondary.dispatch_event(&drag_event("dragover")).unwrap();
        settle().await;

        assert!(
            query(&container, ".pane-drop-overlay").is_none(),
            "a pane that already holds the resource shows no accepting overlay"
        );
        let existing =
            tab_button_named_in(&pane_element(&container, PaneId::Secondary), "alpha.rs");
        assert!(
            existing
                .get_attribute("class")
                .is_some_and(|class| class.contains("tab-drag-conflict")),
            "the occurrence already in the target pane is highlighted"
        );
        assert!(
            live_region_text(&container).contains("already open in the other pane"),
            "the refusal states the state layer's own reason, got {:?}",
            live_region_text(&container)
        );

        // The drop cannot land: dragover never called preventDefault. Even if a
        // drop event is forced, nothing moves and nothing is lost.
        secondary.dispatch_event(&drag_event("drop")).unwrap();
        settle().await;
        assert_eq!(
            panes(&container).len(),
            2,
            "the split survives a refused drag"
        );
        assert_eq!(
            tab_mounts(&container)
                .iter()
                .filter(|mount| text_of(mount).contains("fn alpha()"))
                .count(),
            2,
            "both occurrences survive: nothing was moved or destroyed"
        );

        tab_button_named_in(&pane_element(&container, PaneId::Primary), "alpha.rs")
            .dispatch_event(&drag_event("dragend"))
            .unwrap();
        settle().await;
        assert!(
            query(&container, ".tab-drag-conflict").is_none(),
            "the conflict highlight is cleared when the drag ends"
        );
    }

    /// In narrow mode the other pane is `display: none`. Its chat may own the
    /// composer, but the user cannot see that composer — so "Go to Chat" must
    /// not treat it as the chat they are already typing into and throw them
    /// into a pane they cannot see.
    #[wasm_bindgen_test]
    async fn go_to_chat_ignores_a_composer_hidden_by_narrow_mode() {
        // Below MIN_SPLIT_WIDTH: the real width observer drives narrow mode.
        let container = make_sized_container(500.0, CONTAINER_HEIGHT);
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        // Primary: a file (active) with a chat hidden behind it.
        // Secondary: a chat, active — and therefore the composer owner.
        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab(TabContent::empty_chat(), "Focused Chat".to_owned(), true);
        settle().await;
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "test-host".to_owned(),
                agent_id: protocol::AgentId("hidden".to_owned()),
            }),
            "Hidden Chat".to_owned(),
            true,
        );
        settle().await;
        let primary = pane_element(&container, PaneId::Primary);
        tab_button_named_in(&primary, "alpha.rs").click();
        settle().await;

        // The secondary pane owns the composer, and is not visible.
        let secondary = pane_element(&container, PaneId::Secondary);
        assert!(
            !is_visible(&secondary),
            "precondition: narrow mode hides the unfocused pane"
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.composer_owner().map(|(pane, _)| pane)),
            Some(PaneId::Secondary),
            "precondition: the hidden pane's chat owns the composer"
        );

        crate::components::command_palette::execute_command(
            &state,
            CommandId::GoToChat,
            Some(500.0),
        );
        settle().await;

        assert_eq!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.focused_id()),
            PaneId::Primary,
            "Go to Chat stays in the pane the user can actually see"
        );
        let revealed = state.center_zone.with_untracked(|center_zone| {
            center_zone
                .active_tab()
                .map(|tab| tab.label.clone())
                .unwrap_or_default()
        });
        assert_eq!(
            revealed, "Focused Chat",
            "it reveals the chat in the visible pane, not the one whose composer \
             is hidden by narrow mode"
        );
    }

    /// `dragleave` fires whenever the pointer crosses into a child of the pane.
    /// Clearing the drop target on those would make the overlay flicker and
    /// could drop the target while the pointer is still inside the pane.
    #[wasm_bindgen_test]
    async fn dragging_across_a_panes_children_keeps_the_drop_target() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        state.open_tab_in(
            PaneId::Secondary,
            TabContent::empty_chat(),
            "Chat".to_owned(),
            true,
        );
        settle().await;

        let primary = pane_element(&container, PaneId::Primary);
        let secondary = pane_element(&container, PaneId::Secondary);
        tab_button_named_in(&secondary, "Chat")
            .dispatch_event(&drag_event("dragstart"))
            .unwrap();
        settle().await;
        primary.dispatch_event(&drag_event("dragover")).unwrap();
        settle().await;
        assert!(
            query(&container, ".pane-drop-overlay").is_some(),
            "precondition: the primary pane is an accepting drop target"
        );

        // Cross into a child of the same pane — its content area.
        let inner = query(&primary, ".center-content").expect("pane content");
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_related_target(Some(&inner));
        let leave =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("dragleave", &init).unwrap();
        primary.dispatch_event(&leave).unwrap();
        settle().await;
        assert!(
            query(&container, ".pane-drop-overlay").is_some(),
            "moving onto a child of the pane is not leaving the pane: the drop \
             target must survive"
        );

        // Leaving the window entirely (null relatedTarget) is a real exit.
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        let leave =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("dragleave", &init).unwrap();
        primary.dispatch_event(&leave).unwrap();
        settle().await;
        assert!(
            query(&container, ".pane-drop-overlay").is_none(),
            "a leave with no related target really does exit the pane"
        );
    }

    /// The pre-split net's open bug: with tabs disabled, `replace_active`
    /// swaps a tab's resource under the same `TabId`. Keying the mount on the
    /// bare variant left the *previous* file rendered; keying it on resource
    /// identity remounts.
    #[wasm_bindgen_test]
    async fn tabs_disabled_replacing_a_file_renders_the_new_file() {
        let container = make_container();
        let state = AppState::new();
        state.tabs_enabled.set(false);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            install_tab_lru_effect(&state_for_mount);
            view! { <CenterZone /> }
        });
        settle().await;

        open_file_tab(&state, "alpha.rs", "fn alpha() {}");
        settle().await;
        assert!(
            text_of(&visible_mount(&container)).contains("fn alpha()"),
            "the first file renders"
        );

        open_file_tab(&state, "bravo.rs", "fn bravo() {}");
        settle().await;

        assert_eq!(
            tab_mounts(&container).len(),
            1,
            "tabs-disabled mode still holds exactly one tab"
        );
        let visible = visible_mount(&container);
        assert!(
            text_of(&visible).contains("fn bravo()"),
            "replacing the file must render the new file, got {:?}",
            text_of(&visible)
        );
        assert!(
            !text_of(&visible).contains("fn alpha()"),
            "the replaced file must not still be on screen"
        );
    }
}
