use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::chat_streaming::ChatStreamingView;
use crate::components::settings_panel::persist_tool_output_mode;
use crate::components::task_list::TaskListView;
use crate::state::{
    ActiveAgentRef, AgentInfo, AppState, ChatRowHandle, ChatRowId, TabId, TabScrollState,
    ToolOutputMode, TransientEvent,
};

use protocol::BackendKind;

/// Default per-row height assumed for rows we haven't measured yet.
/// Affects initial scrollbar size and pre-measurement window math; once
/// a row is measured by the per-row `ResizeObserver` the real height
/// supersedes this. Picked to roughly match a typical text-only chat
/// card so first-paint geometry is in the right ballpark for short
/// transcripts.
const ESTIMATED_ROW_HEIGHT: f64 = 200.0;
/// Number of rows to render outside the visible viewport in each
/// direction. A small buffer means scroll-into-view shows a measured row
/// rather than a default-sized placeholder, hiding the first-frame
/// height correction from the user.
const VIRT_OVERSCAN: usize = 5;
/// CSS gap inserted between adjacent rows by `.virt-row + .virt-row {
/// margin-top: 6px; }` in styles.css. `ResizeObserver` reports the
/// row's own border-box height — it doesn't include outside margins —
/// so the spacer/scroll math has to add this back per non-first row,
/// otherwise the scrollbar drifts (under-reports total content height
/// by `ROW_GAP_PX` per unmounted gap on long transcripts). Must stay
/// in lockstep with the CSS rule.
const ROW_GAP_PX: f64 = 6.0;

/// How many trailing messages stay visible when a long history is collapsed
/// on restore. Render only the very last message initially.
const HISTORY_INITIAL_VISIBLE: usize = 1;
/// Only collapse genuinely-long histories — short or actively-growing chats
/// render in full, so a normal conversation or a live reconnect is never
/// needlessly hidden behind the "Load previous" control.
const HISTORY_COLLAPSE_THRESHOLD: usize = 20;

/// First row index the chat view renders. Returns 0 (render everything)
/// unless the transcript is longer than [`HISTORY_COLLAPSE_THRESHOLD`], in
/// which case all but the last [`HISTORY_INITIAL_VISIBLE`] messages are
/// hidden behind the "Load previous conversation history" control. The AI
/// always keeps the full conversation in context; this only windows the view.
pub fn initial_history_floor(total: usize) -> usize {
    if total > HISTORY_COLLAPSE_THRESHOLD {
        total.saturating_sub(HISTORY_INITIAL_VISIBLE)
    } else {
        0
    }
}

fn tab_scroll_state_from_element(el: &web_sys::Element, user_scrolled_up: bool) -> TabScrollState {
    TabScrollState {
        scroll_top: el.scroll_top(),
        scroll_height: el.scroll_height(),
        client_height: el.client_height(),
        user_scrolled_up,
    }
}

fn restore_scroll_top_without_animation(el: &web_sys::HtmlElement, scroll_top: i32) {
    let style = el.style();
    let previous = style.get_property_value("scroll-behavior").ok();
    let _ = style.set_property("scroll-behavior", "auto");
    el.set_scroll_top(scroll_top);
    leptos::prelude::set_timeout(
        move || match previous.as_deref() {
            Some(value) if !value.is_empty() => {
                let _ = style.set_property("scroll-behavior", value);
            }
            _ => {
                let _ = style.remove_property("scroll-behavior");
            }
        },
        std::time::Duration::from_millis(0),
    );
}

/// Feature-discovery tips shown on empty chat drafts, keyed by tab id so
/// each new chat surfaces the next one instead of repeating at random.
const DID_YOU_KNOW_TIPS: &[(&str, &str)] = &[
    (
        "Multi-backend orchestration",
        "Pick the Orchestrator agent from the New Chat \u{25be} menu: every backend drafts a plan, the plans cross-review to consensus, one agent implements, and the other backends review the result.",
    ),
    (
        "Ask the Help agent",
        "Pick Help from the New Chat \u{25be} menu to ask how anything in Tyde works \u{2014} it can change settings and create agents for you.",
    ),
    (
        "Customize your default agent",
        "Edit the Default agent in Settings \u{2192} Custom Agents to shape every chat that doesn't pick a specific agent.",
    ),
    (
        "Task complexity tiers",
        "Turn on tiers in Settings \u{2192} Backends to run cheap fast agents for small tasks and maximum-power agents for hard ones.",
    ),
    (
        "Agent teams",
        "The Teams panel builds a manager-plus-specialists roster that plans, implements, and reviews on your behalf.",
    ),
    (
        "Command palette",
        "\u{2318}K searches everything you can do in Tyde \u{2014} switching projects, opening panels, starting chats.",
    ),
    (
        "Skills and steering",
        "Settings \u{2192} Skills and Steering inject reusable guidance into every agent you spawn.",
    ),
    (
        "Tyde on your phone",
        "Pair a phone in Settings \u{2192} Mobile to watch and steer agents away from your desk.",
    ),
];

#[component]
pub fn ChatView(
    tab_id: TabId,
    /// Per-instance binding to a chat — typically derived from a tab's
    /// `TabContent::Chat { agent_ref }` so each tab has its own view that
    /// stays mounted even when the tab is hidden via CSS. Passed as a Signal
    /// so the view tracks the rare in-place mutation where a "New Chat" tab's
    /// agent_ref upgrades from `None` to the spawned agent (see
    /// `dispatch.rs` agent-creation handling).
    agent_ref: Signal<Option<ActiveAgentRef>>,
    /// True only when this tab is the active one in the center-zone.
    /// Used to gate the `ChatInput` so hidden chat tabs don't mount
    /// duplicate inputs that all subscribe to the global
    /// `state.chat_input` — every keystroke would wake each hidden
    /// instance, doubling-or-worse the per-keystroke main-thread cost.
    is_active: Signal<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let initial_scroll_state = state.tab_scroll_state_untracked(tab_id);

    let has_agent = move || agent_ref.get().is_some();

    // Reactive identifier of the chat the row list belongs to. Combined with
    // `idx` it forms the keyed `<For>` row identity below: switching agents
    // changes every key (clean remount), appending a message preserves rows
    // 0..len() and only mounts the new tail row.
    let active_agent_id = move || agent_ref.get().map(|a| a.agent_id);

    let messages_len: Memo<usize> = Memo::new(move |_| match active_agent_id() {
        Some(id) => state
            .chat_rows
            .with(|m| m.get(&id).map(|v| v.len()).unwrap_or(0)),
        None => 0,
    });

    let row_handles = move || -> Vec<ChatRowHandle> {
        let Some(id) = active_agent_id() else {
            return Vec::new();
        };
        state
            .chat_rows
            .with(|m| m.get(&id).cloned().unwrap_or_default())
    };

    // Absolute index of the first row the view renders, clamped to the row
    // count so a stale floor can never slice out of bounds. 0 means "render
    // the whole history"; a positive value collapses everything before it
    // behind the "Load previous conversation history" control.
    let history_floor = move || -> usize {
        let Some(id) = active_agent_id() else {
            return 0;
        };
        let total = state
            .chat_rows
            .with(|m| m.get(&id).map(|v| v.len()).unwrap_or(0));
        state
            .history_floor
            .with(|m| m.get(&id).copied().unwrap_or(0))
            .min(total)
    };

    // The rows actually fed to the windowed `<For>`: the full history minus
    // everything below the floor. Auto-scroll and `messages_len` deliberately
    // stay on the full count so restoring still pins to the true bottom.
    let revealed_handles = move || -> Vec<ChatRowHandle> {
        let rows = row_handles();
        let floor = history_floor().min(rows.len());
        rows[floor..].to_vec()
    };

    // True when earlier messages are hidden behind the load-previous control.
    let history_collapsed = move || history_floor() > 0;

    // Reveal the collapsed history: drop the floor to 0 and stop tail-tracking
    // so later messages don't re-collapse the view.
    let reveal_history = move |_| {
        let Some(id) = active_agent_id() else {
            return;
        };
        state.history_floor.update(|m| {
            m.insert(id.clone(), 0);
        });
        state.history_settling.update(|s| {
            s.remove(&id);
        });
    };

    // `.with` reads through the HashMap signals without cloning the
    // entire map — the previous `.get()` allocated a fresh
    // HashMap<AgentId, StreamingState> on every read, and these
    // closures fire from the auto-scroll Effect on every stream-start
    // / stream-end, plus per-render in the streaming-card branch.
    let streaming = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.streaming_text.with(|m| m.get(&agent_id).cloned())
    };

    let task_list = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.task_lists.with(|m| m.get(&agent_id).cloned())
    };

    // Walk back from the latest message to find the most recent assistant
    // message that carries a context_breakdown. `ContextBreakdown` does not
    // implement `PartialEq`, so we use a derived Signal rather than a Memo.
    // Each read still walks the vec, but it's bounded by "messages up to the
    // most recent assistant turn" — typically a single iteration.
    let context_breakdown: Signal<Option<protocol::ContextBreakdown>> = Signal::derive(move || {
        let id = active_agent_id()?;
        state.chat_rows.with(|m| {
            let rows = m.get(&id)?;
            for row in rows.iter().rev() {
                let entry = row.entry.get();
                let is_assistant = matches!(
                    entry.message.sender,
                    protocol::MessageSender::Assistant { .. }
                );
                if !is_assistant {
                    continue;
                }
                if let Some(breakdown) = entry.message.context_breakdown.clone() {
                    return Some(breakdown);
                }
                if entry.message.tool_calls.is_empty() {
                    return None;
                }
            }
            None
        })
    });

    let transient_events = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.transient_events.with(|m| m.get(&agent_id).cloned())
    };

    // Centralised lookup of the AgentInfo for this view's agent_ref.
    // The previous code did `state.agents.get()` (clones the full Vec)
    // three times across `agent_name`, `agent_backend`, and
    // `agent_initializing`, so any agent-list change fired three full
    // clones. Sharing a single `Memo<Option<AgentInfo>>` collapses
    // that to one clone per change, with closures becoming cheap
    // field reads.
    let current_agent: Memo<Option<AgentInfo>> = Memo::new(move |_| {
        let active = agent_ref.get()?;
        state.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == active.host_id && a.agent_id == active.agent_id)
                .cloned()
        })
    });

    let agent_name = move || -> String {
        if agent_ref.get().is_none() {
            return String::new();
        }
        current_agent
            .get()
            .map(|a| a.name)
            .unwrap_or_else(|| "[unknown agent]".to_owned())
    };

    let agent_backend =
        move || -> Option<BackendKind> { current_agent.get().map(|a| a.backend_kind) };

    let agent_initializing = move || -> bool {
        current_agent
            .get()
            .map(|a| !a.started && a.fatal_error.is_none())
            .unwrap_or(false)
    };

    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    let user_scrolled_up =
        RwSignal::new(initial_scroll_state.is_some_and(|scroll| scroll.user_scrolled_up));
    let show_scroll_btn =
        RwSignal::new(initial_scroll_state.is_some_and(|scroll| scroll.user_scrolled_up));
    let view_mounted = Arc::new(AtomicBool::new(true));
    let view_mounted_for_cleanup = view_mounted.clone();
    on_cleanup(move || {
        view_mounted_for_cleanup.store(false, Ordering::Relaxed);
    });

    // Virtualization plumbing — see `VirtualWindow` and the windowed `<For>`
    // below. The chat row list is windowed: only rows whose offsets fall
    // within (scroll_top - overscan, scroll_top + viewport + overscan) are
    // mounted; rows outside the window are summarised by spacer divs.
    //
    // - `scroll_top_sig` and `viewport_height_sig` track the viewport so
    //   the window-computing Memo can react to scroll and resize.
    // - `row_heights` maps `ChatRowId` to the row's measured DOM height.
    //   Rows without an entry use `ESTIMATED_ROW_HEIGHT`. Stored as a
    //   non-reactive `StoredValue` because it can churn at high frequency
    //   during streaming and its updates are signalled coarsely via
    //   `heights_version`.
    // - `heights_version` is bumped any time `row_heights` mutates by a
    //   meaningful amount; the windowing Memo subscribes to it.
    let scroll_top_sig =
        RwSignal::new(initial_scroll_state.map_or(0.0_f64, |scroll| scroll.scroll_top as f64));
    let viewport_height_sig = RwSignal::new(800.0_f64);
    let row_heights: StoredValue<HashMap<ChatRowId, f64>, LocalStorage> =
        StoredValue::new_local(HashMap::new());
    let heights_version = RwSignal::new(0u32);

    let restored_initial_scroll = std::rc::Rc::new(std::cell::Cell::new(false));
    let restored_initial_scroll_for_effect = restored_initial_scroll.clone();
    let scroll_ref_for_restore = scroll_ref;
    let state_for_restore = state.clone();
    Effect::new(move |_| {
        if restored_initial_scroll_for_effect.get() {
            return;
        }
        let Some(el) = scroll_ref_for_restore.get() else {
            return;
        };
        // A collapsed history (floor > 0) makes any saved full-history scroll
        // offset meaningless: the rows it pointed at aren't rendered. Pin to
        // the bottom (last message) and start in sticky-bottom mode instead.
        let collapsed = history_floor() > 0;
        let saved = initial_scroll_state;
        if saved.is_none() && !collapsed {
            return;
        }
        restored_initial_scroll_for_effect.set(true);
        let restore_user_scrolled_up =
            saved.is_some_and(|scroll| scroll.user_scrolled_up) && !collapsed;
        let target_scroll_top = if restore_user_scrolled_up {
            saved.map(|scroll| scroll.scroll_top).unwrap_or(0)
        } else {
            el.scroll_height()
        };
        if collapsed {
            user_scrolled_up.set(false);
            show_scroll_btn.set(false);
        }
        let html_el: web_sys::HtmlElement = el.clone().unchecked_into();
        restore_scroll_top_without_animation(&html_el, target_scroll_top);
        scroll_top_sig.set(html_el.scroll_top() as f64);
        state_for_restore.save_tab_scroll_state(
            tab_id,
            TabScrollState {
                scroll_top: html_el.scroll_top(),
                scroll_height: html_el.scroll_height(),
                client_height: html_el.client_height(),
                user_scrolled_up: restore_user_scrolled_up,
            },
        );
    });

    // Per-instance scroll + user-input listeners. Multiple `ChatView`s
    // can be mounted simultaneously (LRU hot set), so we can't use
    // thread-local handles. Closures are parked in a `StoredValue`
    // and removed on `on_cleanup` — tab LRU eviction can mount/unmount
    // this ChatView many times for the same chat, and without explicit
    // cleanup each cycle would leak its handlers.
    struct ScrollListenerHolder {
        element: web_sys::HtmlElement,
        scroll_handler: Closure<dyn Fn()>,
        input_handler: Closure<dyn Fn()>,
    }
    let scroll_listener_slot: StoredValue<Option<ScrollListenerHolder>, LocalStorage> =
        StoredValue::new_local(None);
    let view_mounted_for_listeners = view_mounted.clone();
    // Two listeners, with separate responsibilities:
    //
    //   1. The `scroll` listener (always fires, including on
    //      programmatic `set_scroll_top` calls). It updates
    //      `scroll_top_sig` — the windowing Memo needs current scroll
    //      position. When `scrollTop` actually moves, it also updates
    //      `user_scrolled_up`; this catches scrollbar/page-script
    //      scrolls that do not emit wheel/touch/key events. Scroll
    //      events without `scrollTop` movement still leave sticky-bottom
    //      alone, so content growing below the user (e.g. during a
    //      session restore where messages stream in over seconds)
    //      cannot masquerade as user intent and disable sticky-bottom.
    //
    //   2. The user-input listeners (`wheel`, `touchstart`, `keydown`)
    //      fire only on real user actions. Those re-evaluate distance-
    //      from-bottom and update `user_scrolled_up` / `show_scroll_btn`
    //      accordingly. Programmatic scrolls and content-growth scrolls
    //      stay sticky.
    let scroll_ref_for_handler = scroll_ref;
    let state_for_scroll_listener = state.clone();
    Effect::new(move |_| {
        let Some(el) = scroll_ref_for_handler.get() else {
            return;
        };
        if scroll_listener_slot.with_value(|s| s.is_some()) {
            return;
        }
        let el_clone = el.clone();
        let state_for_scroll_handler = state_for_scroll_listener.clone();
        let listener_pending = std::rc::Rc::new(std::cell::Cell::new(false));
        let listener_mounted = view_mounted_for_listeners.clone();
        let scroll_handler = Closure::<dyn Fn()>::new(move || {
            let scroll_top = el_clone.scroll_top() as f64;
            let scroll_changed = (scroll_top_sig.get_untracked() - scroll_top).abs() >= 1.0;
            if scroll_changed {
                scroll_top_sig.set(scroll_top);
                let distance_from_bottom =
                    el_clone.scroll_height() - el_clone.scroll_top() - el_clone.client_height();
                let is_near_bottom = distance_from_bottom < 80;
                user_scrolled_up.set(!is_near_bottom);
                show_scroll_btn.set(!is_near_bottom);
            }
            if listener_pending.get() {
                return;
            }
            listener_pending.set(true);
            let pending = listener_pending.clone();
            let el_for_cb = el_clone.clone();
            let state_for_cb = state_for_scroll_handler.clone();
            let mounted = listener_mounted.clone();
            // `setTimeout(0)` instead of `requestAnimationFrame` — rAF
            // is paused for hidden Tauri webviews (macOS WKWebView
            // throttles when the window is occluded). setTimeout
            // fires regardless of visibility.
            leptos::prelude::set_timeout(
                move || {
                    if !mounted.load(Ordering::Relaxed) {
                        return;
                    }
                    pending.set(false);
                    let scroll_top = el_for_cb.scroll_top();
                    scroll_top_sig.set(scroll_top as f64);
                    let element: web_sys::Element = el_for_cb.clone().unchecked_into();
                    state_for_cb.save_tab_scroll_state(
                        tab_id,
                        tab_scroll_state_from_element(&element, user_scrolled_up.get_untracked()),
                    );
                },
                std::time::Duration::from_millis(0),
            );
        });
        let _ =
            el.add_event_listener_with_callback("scroll", scroll_handler.as_ref().unchecked_ref());

        // User-input observation. Each user-input event re-evaluates
        // distance-from-bottom and updates `user_scrolled_up`. The
        // events themselves don't carry post-scroll geometry — we
        // schedule a `setTimeout(0)` to read after the browser has
        // applied the input's scroll effect.
        let el_for_input = el.clone();
        let state_for_input_handler = state_for_scroll_listener.clone();
        let input_pending = std::rc::Rc::new(std::cell::Cell::new(false));
        let input_mounted = view_mounted_for_listeners.clone();
        let input_handler = Closure::<dyn Fn()>::new(move || {
            if input_pending.get() {
                return;
            }
            input_pending.set(true);
            let pending = input_pending.clone();
            let el_for_cb = el_for_input.clone();
            let state_for_cb = state_for_input_handler.clone();
            let mounted = input_mounted.clone();
            leptos::prelude::set_timeout(
                move || {
                    if !mounted.load(Ordering::Relaxed) {
                        return;
                    }
                    pending.set(false);
                    let scroll_height = el_for_cb.scroll_height();
                    let scroll_top = el_for_cb.scroll_top();
                    let client_height = el_for_cb.client_height();
                    let distance_from_bottom = scroll_height - scroll_top - client_height;
                    let is_near_bottom = distance_from_bottom < 80;
                    user_scrolled_up.set(!is_near_bottom);
                    show_scroll_btn.set(!is_near_bottom);
                    let element: web_sys::Element = el_for_cb.clone().unchecked_into();
                    state_for_cb.save_tab_scroll_state(
                        tab_id,
                        tab_scroll_state_from_element(&element, !is_near_bottom),
                    );
                },
                std::time::Duration::from_millis(0),
            );
        });
        for event in &["wheel", "touchstart", "keydown"] {
            let _ =
                el.add_event_listener_with_callback(event, input_handler.as_ref().unchecked_ref());
        }

        let element: web_sys::HtmlElement = el.unchecked_into();
        scroll_listener_slot.update_value(|s| {
            *s = Some(ScrollListenerHolder {
                element,
                scroll_handler,
                input_handler,
            })
        });
    });
    let state_for_scroll_cleanup = state.clone();
    on_cleanup(move || {
        scroll_listener_slot.update_value(|s| {
            if let Some(holder) = s.take() {
                let element: web_sys::Element = holder.element.clone().unchecked_into();
                state_for_scroll_cleanup.save_tab_scroll_state(
                    tab_id,
                    tab_scroll_state_from_element(&element, user_scrolled_up.get_untracked()),
                );
                let _ = holder.element.remove_event_listener_with_callback(
                    "scroll",
                    holder.scroll_handler.as_ref().unchecked_ref(),
                );
                for event in &["wheel", "touchstart", "keydown"] {
                    let _ = holder.element.remove_event_listener_with_callback(
                        event,
                        holder.input_handler.as_ref().unchecked_ref(),
                    );
                }
                // Closures drop here.
            }
        });
    });

    // Track viewport height via `ResizeObserver` on the scroll container.
    // The window-bounds Memo needs the live height, not just whatever
    // happened to be true at first paint. The observer also fires when
    // the user resizes the window or toggles dock visibility, both of
    // which affect what's actually visible.
    type ViewportObserverSlot = Option<(
        web_sys::ResizeObserver,
        Closure<dyn FnMut(JsValue, JsValue)>,
    )>;
    let viewport_observer_slot: StoredValue<ViewportObserverSlot, LocalStorage> =
        StoredValue::new_local(None);
    let scroll_ref_for_viewport = scroll_ref;
    let view_mounted_for_viewport = view_mounted.clone();
    Effect::new(move |_| {
        let Some(el) = scroll_ref_for_viewport.get() else {
            return;
        };
        if viewport_observer_slot.with_value(|s| s.is_some()) {
            return;
        }
        // Seed the signal eagerly so the first paint gets a real value
        // rather than the default 800px estimate.
        viewport_height_sig.set(el.client_height() as f64);
        let el_clone = el.clone();
        let viewport_pending = std::rc::Rc::new(std::cell::Cell::new(false));
        let viewport_mounted = view_mounted_for_viewport.clone();
        let cb =
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_entries: JsValue, _: JsValue| {
                if viewport_pending.get() {
                    return;
                }
                viewport_pending.set(true);
                let pending = viewport_pending.clone();
                let el_for_cb = el_clone.clone();
                let mounted = viewport_mounted.clone();
                leptos::prelude::set_timeout(
                    move || {
                        if !mounted.load(Ordering::Relaxed) {
                            return;
                        }
                        pending.set(false);
                        viewport_height_sig.set(el_for_cb.client_height() as f64);
                    },
                    std::time::Duration::from_millis(0),
                );
            });
        if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            let element: web_sys::Element = el.unchecked_into();
            observer.observe(&element);
            viewport_observer_slot.update_value(|s| *s = Some((observer, cb)));
        }
    });
    on_cleanup(move || {
        viewport_observer_slot.update_value(|s| {
            if let Some((observer, _cb)) = s.take() {
                observer.disconnect();
            }
        });
    });

    // Compute the row index window plus top/bottom spacer heights.
    // Reactive on `chat_rows` (via `row_handles`), scroll position,
    // viewport height, and `heights_version` (per-row measurements).
    // Returns indices into the *current* rows Vec.
    //
    // Algorithm: walk forward summing per-row heights until we cross
    // `scroll_top` (first visible) and again until we cross
    // `scroll_top + viewport` (one past last visible). Apply
    // `VIRT_OVERSCAN` rows of buffer in each direction so a row at the
    // edge isn't visibly missing while it's being measured.
    let visible_window: Memo<VirtualWindow> = Memo::new(move |_| {
        let _ = heights_version.get();
        let st = scroll_top_sig.get();
        let vp = viewport_height_sig.get();
        let rows = revealed_handles();
        let n = rows.len();
        if n == 0 {
            return VirtualWindow::EMPTY;
        }
        row_heights.with_value(|map| {
            // `slot_height` includes the top margin that separates this
            // row from the previous one, so the running sum matches
            // what the browser actually lays out. The very first row
            // gets no leading gap.
            let slot_height = |idx: usize| -> f64 {
                let raw = map
                    .get(&rows[idx].id)
                    .copied()
                    .unwrap_or(ESTIMATED_ROW_HEIGHT);
                if idx == 0 { raw } else { raw + ROW_GAP_PX }
            };

            let mut acc = 0.0_f64;
            let mut first = 0usize;
            while first < n {
                let h = slot_height(first);
                if acc + h > st {
                    break;
                }
                acc += h;
                first += 1;
            }
            let viewport_end = st + vp;
            let mut last_excl = first;
            while last_excl < n {
                if acc >= viewport_end {
                    break;
                }
                acc += slot_height(last_excl);
                last_excl += 1;
            }
            let start = first.saturating_sub(VIRT_OVERSCAN);
            let end = (last_excl + VIRT_OVERSCAN).min(n);
            let top_pad: f64 = (0..start).map(slot_height).sum();
            let bottom_pad: f64 = (end..n).map(slot_height).sum();
            VirtualWindow {
                start,
                end,
                top_pad,
                bottom_pad,
            }
        })
    });

    // Auto-scroll effect: whenever the message count or streaming text grows,
    // scroll to bottom (only if the user has scrolled up). Scoped to the
    // *length* of messages — not the full Vec — so unrelated chat row
    // updates (e.g. tool_request mutations to existing rows) don't trigger a
    // scroll.
    //
    // Coalesce multiple deltas-per-frame into a single setTimeout. The
    // previous implementation scheduled one rAF per `text`/`reasoning`
    // delta — at 50+ deltas/sec while the model streams, all of them
    // fired in the *same* frame and each ran its own scrollHeight read
    // (a forced layout) plus a scrollTop write. The pending-flag gate
    // caps it to at most one scroll per coalesced burst, which still
    // keeps the bottom pinned.
    //
    // Subscribes to `heights_version` so a measurement that grew the last
    // (visible/streaming) row's height re-pins the bottom. Without that
    // subscription, sticky-bottom would visibly drift up by the height
    // delta on every measurement during streaming.
    //
    // `user_scrolled_up` is set true only by the user-input listeners
    // below (wheel/touchstart/keydown). The plain `scroll` event never
    // touches it, so content growing below the user can't masquerade
    // as user intent and disable sticky-bottom.
    let scroll_pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let view_mounted_for_auto_scroll = view_mounted.clone();
    let state_for_auto_scroll = state.clone();
    Effect::new(move |_| {
        let _len = messages_len.get();
        let _hv = heights_version.get();
        let stream = streaming();
        if let Some(ss) = stream.as_ref() {
            // Subscribe without cloning the strings. `.get()` on
            // `ArcRwSignal<String>` cloned the entire accumulated text
            // into a temporary just to be discarded — `.with` reads
            // through and tracks the dependency without the alloc.
            ss.text.with(|_| ());
            ss.reasoning.with(|_| ());
        }
        if user_scrolled_up.get_untracked() {
            return;
        }
        if scroll_pending.get() {
            return;
        }
        // Resolve the NodeRef synchronously — the Effect body runs
        // inside this component's reactive owner, so the signal is
        // guaranteed alive here. Capturing the raw `HtmlDivElement`
        // into the deferred closure means the timer never touches the
        // reactive graph after the owner is disposed (tab LRU eviction
        // mid-flight used to panic here).
        let Some(el) = scroll_ref.get_untracked() else {
            return;
        };
        scroll_pending.set(true);
        let pending = scroll_pending.clone();
        let mounted = view_mounted_for_auto_scroll.clone();
        let state_for_cb = state_for_auto_scroll.clone();
        // `setTimeout(0)` instead of `requestAnimationFrame`. rAF is
        // paused for hidden Tauri windows on macOS — a user
        // backgrounding the app during session restore would leave the
        // chat stuck wherever it was. setTimeout fires regardless of
        // window visibility. We still coalesce within a reactive batch
        // via `scroll_pending`.
        leptos::prelude::set_timeout(
            move || {
                if !mounted.load(Ordering::Relaxed) {
                    return;
                }
                pending.set(false);
                el.set_scroll_top(el.scroll_height());
                // Mirror the post-clamp scrollTop into `scroll_top_sig`
                // immediately. Without this, the windowing Memo only
                // sees the new scroll position once the `scroll` event
                // round-trips through the listener — leaving a window
                // of one or more frames where `scroll_top` is at the
                // bottom but `visible_window` still has the old
                // `start = 0`. The user would see the scrollbar at the
                // end but the rendered rows from index 0, with the
                // bottom-pad spacer covering the entire visible region.
                scroll_top_sig.set(el.scroll_top() as f64);
                let element: web_sys::Element = el.clone().unchecked_into();
                state_for_cb
                    .save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element, false));
            },
            std::time::Duration::from_millis(0),
        );
    });

    let tab_scroll_state_for_scroll_to_bottom = state.tab_scroll_state;
    let scroll_to_bottom = move |_| {
        // Event handler — not a reactive context, so use untracked
        // read on the NodeRef.
        if let Some(el) = scroll_ref.get_untracked() {
            el.set_scroll_top(el.scroll_height());
            // Same staleness fix as the auto-scroll rAF — keep
            // `scroll_top_sig` synchronously consistent with the new
            // scroll position so the windowing Memo recomputes
            // immediately rather than waiting on the scroll event.
            scroll_top_sig.set(el.scroll_top() as f64);
            user_scrolled_up.set(false);
            show_scroll_btn.set(false);
            let element: web_sys::Element = el.clone().unchecked_into();
            tab_scroll_state_for_scroll_to_bottom.update(|scroll| {
                scroll.insert(tab_id, tab_scroll_state_from_element(&element, false));
            });
        }
    };

    let has_messages = move || messages_len.get() > 0;

    // (ToolOutputModeToggle is defined below.)

    view! {
        <div class="chat-view">
          <div class="chat-view-body">
            <div class="chat-view-main">
            <Show
                when=has_agent
                fallback=move || {
                    view! {
                        <div class="chat-welcome">
                            <div class="chat-welcome-inner">
                                <img class="chat-welcome-icon" src="icon.png" alt="Tyde" />
                                <h2 class="chat-welcome-title">"Tyde"</h2>
                                <p class="chat-welcome-subtitle">"Send a message to start a conversation"</p>
                                <div class="chat-didyouknow">
                                    <span class="chat-didyouknow-label">"Did you know?"</span>
                                    <div class="chat-didyouknow-title">
                                        {DID_YOU_KNOW_TIPS[tab_id.0 as usize % DID_YOU_KNOW_TIPS.len()].0}
                                    </div>
                                    <p class="chat-didyouknow-body">
                                        {DID_YOU_KNOW_TIPS[tab_id.0 as usize % DID_YOU_KNOW_TIPS.len()].1}
                                    </p>
                                </div>
                                <div class="chat-welcome-shortcuts">
                                    <span class="chat-welcome-shortcut"><kbd>"Enter"</kbd>" Send Message"</span>
                                    <span class="chat-welcome-shortcut"><kbd>"Ctrl+K"</kbd>" Command Palette"</span>
                                </div>
                            </div>
                        </div>
                    }
                }
            >
                <div class="chat-agent-header">
                    <span class="chat-agent-name">{agent_name}</span>
                    {move || agent_backend().map(|kind| {
                        let (badge_class, label) = match kind {
                            BackendKind::Tycode => ("backend-badge tycode", "Tycode"),
                            BackendKind::Kiro => ("backend-badge kiro", "Kiro"),
                            BackendKind::Claude => ("backend-badge claude", "Claude"),
                            BackendKind::Codex => ("backend-badge codex", "Codex"),
                            BackendKind::Antigravity => ("backend-badge antigravity", "Antigravity"),
                        };
                        view! { <span class=badge_class>{label}</span> }
                    })}
                    <ToolOutputModeToggle />
                    <ReviewChangesButton />
                </div>
                {move || {
                    view! {
                        <TaskListView
                            task_list=task_list()
                            context_breakdown=context_breakdown.get()
                        />
                    }
                }}
                <Show when=agent_initializing>
                    <div class="chat-initializing-overlay">
                        <div class="chat-initializing-spinner"></div>
                        <p class="chat-initializing-text">"Initializing agent\u{2026}"</p>
                    </div>
                </Show>
                <div class="chat-messages-wrapper">
                    <div class="chat-messages" node_ref=scroll_ref>
                        {move || {
                            if !has_messages() && streaming().is_none() && !agent_initializing() {
                                Some(view! {
                                    <div class="chat-empty-hint">
                                        <p>"Type a message to start the conversation"</p>
                                    </div>
                                })
                            } else {
                                None
                            }
                        }}

                        // Windowed history: top spacer + visible rows +
                        // bottom spacer. The spacers reserve scroll
                        // geometry for the unrendered rows so the
                        // scrollbar tracks total estimated height even
                        // though we only mount what's near the viewport.
                        // `MeasuredRow` reports each rendered row's
                        // post-layout height back into `row_heights`,
                        // which keeps the spacers honest as the user
                        // scrolls into previously-unmeasured regions.
                        <Show when=history_collapsed>
                            <div class="chat-history-collapsed">
                                <button
                                    class="chat-history-load-previous"
                                    on:click=reveal_history
                                >
                                    "Load previous conversation history"
                                </button>
                                <p class="chat-history-collapsed-note">
                                    "Earlier messages are hidden from view \u{2014} the AI still has the full conversation in context."
                                </p>
                            </div>
                        </Show>
                        <div
                            class="virt-spacer virt-spacer-top"
                            style=move || {
                                visible_window
                                    .with(|w| format!("height: {}px;", w.top_pad))
                            }
                        ></div>
                        <For
                            each=move || {
                                let win = visible_window.get();
                                let rows = revealed_handles();
                                let end = win.end.min(rows.len());
                                let start = win.start.min(end);
                                rows[start..end].to_vec()
                            }
                            key=|row| row.id
                            let:row
                        >
                            <MeasuredRow
                                agent_ref=agent_ref
                                row=row
                                row_heights=row_heights
                                heights_version=heights_version
                            />
                        </For>
                        <div
                            class="virt-spacer virt-spacer-bottom"
                            style=move || {
                                visible_window
                                    .with(|w| format!("height: {}px;", w.bottom_pad))
                            }
                        ></div>

                        // Transient events (retry, cancel) rendered as cards
                        {move || {
                            transient_events().map(|events| {
                                events.into_iter().map(|ev| {
                                    match ev {
                                        TransientEvent::OperationCancelled { message } => {
                                            view! {
                                                <div class="chat-card chat-card-system chat-card-cancelled">
                                                    <div class="chat-card-header">
                                                        <span class="chat-card-sender">"Cancelled"</span>
                                                    </div>
                                                    <div class="chat-card-body">
                                                        <p class="md-paragraph">{message}</p>
                                                    </div>
                                                </div>
                                            }.into_any()
                                        }
                                        TransientEvent::RetryAttempt { attempt, max_retries, error, backoff_ms } => {
                                            view! {
                                                <div class="chat-card chat-card-retry">
                                                    <div class="retry-card-header">
                                                        <span class="retry-card-icon">"⏳"</span>
                                                        <span class="retry-card-title">"Rate Limited"</span>
                                                        <span class="retry-card-attempt">{format!("Attempt {attempt} of {max_retries}")}</span>
                                                    </div>
                                                    <div class="retry-card-body">
                                                        <p class="retry-card-error">{error}</p>
                                                        <p class="retry-card-countdown">{format!("Retrying in {backoff_ms}ms\u{2026}")}</p>
                                                    </div>
                                                </div>
                                            }.into_any()
                                        }
                                    }
                                }).collect::<Vec<_>>()
                            })
                        }}

                        {move || {
                            streaming().map(|ss| view! { <ChatStreamingView agent_ref=agent_ref streaming=ss /> })
                        }}
                    </div>

                    // Scroll-to-bottom button
                    <Show when=move || show_scroll_btn.get()>
                        <button
                            class="scroll-to-bottom-btn"
                            on:click=scroll_to_bottom
                            title="Scroll to bottom"
                        >
                            <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
                                <path d="M8 3L8 13M8 13L3 8M8 13L13 8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                            </svg>
                        </button>
                    </Show>
                </div>
            </Show>
            <Show
                when=move || is_active.get()
                fallback=|| ()
            >
                <ChatInput />
            </Show>
            </div>
          </div>
        </div>
    }
}

/// Window descriptor produced by the chat-list virtualizer. `start..end`
/// is the half-open range of row indices currently mounted; `top_pad`
/// and `bottom_pad` are the spacer-div heights that reserve scroll
/// geometry for the unmounted rows above and below. `PartialEq` so the
/// `Memo` short-circuits when the window doesn't actually change —
/// avoids triggering downstream re-renders on every signal tick.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VirtualWindow {
    start: usize,
    end: usize,
    top_pad: f64,
    bottom_pad: f64,
}

impl VirtualWindow {
    const EMPTY: Self = Self {
        start: 0,
        end: 0,
        top_pad: 0.0,
        bottom_pad: 0.0,
    };
}

/// Wraps a `ChatMessageView` with a `ResizeObserver` that records the
/// row's measured DOM height into `row_heights` and bumps
/// `heights_version` when the height changes meaningfully (>=0.5px).
/// The bump triggers `visible_window` to recompute, which keeps the
/// top/bottom spacers honest as the user scrolls into rows that were
/// previously estimated.
///
/// We hold the observer alive in an `Rc<RefCell<Option<...>>>` and
/// disconnect it on `on_cleanup` so the GC doesn't collect the closure
/// while the row is still mounted. Per-row observer cost is bounded
/// because at most `viewport / min_row_height + 2 * VIRT_OVERSCAN` rows
/// are mounted at any time.
#[component]
fn MeasuredRow(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    row: ChatRowHandle,
    row_heights: StoredValue<HashMap<ChatRowId, f64>, LocalStorage>,
    heights_version: RwSignal<u32>,
) -> impl IntoView {
    let row_id = row.id;
    let node_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    let row_mounted = Arc::new(AtomicBool::new(true));

    // Observer + closure are !Send/!Sync (web_sys handles wrap raw JS
    // pointers), so we can't capture them in a `Send + Sync` cleanup
    // closure directly. `StoredValue::new_local` parks them in
    // thread-local storage and hands back a `Copy` ID handle that *is*
    // `Send + Sync`. Both the Effect and `on_cleanup` get their own
    // handle that points at the same slot.
    type ObserverPair = Option<(
        web_sys::ResizeObserver,
        Closure<dyn FnMut(JsValue, JsValue)>,
    )>;
    let slot: StoredValue<ObserverPair, LocalStorage> = StoredValue::new_local(None);
    let row_mounted_for_observer = row_mounted.clone();

    Effect::new(move |_| {
        let Some(el) = node_ref.get() else {
            return;
        };
        // Observer already wired? Don't double-wrap.
        let already = slot.with_value(|s| s.is_some());
        if already {
            return;
        }
        let element: web_sys::Element = el.clone().unchecked_into();
        let elem_for_cb = element.clone();
        let resize_pending = std::rc::Rc::new(std::cell::Cell::new(false));
        let row_mounted_for_cb = row_mounted_for_observer.clone();
        let cb =
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_entries: JsValue, _: JsValue| {
                if resize_pending.get() {
                    return;
                }
                resize_pending.set(true);
                let pending = resize_pending.clone();
                let elem_for_timeout = elem_for_cb.clone();
                let mounted = row_mounted_for_cb.clone();
                leptos::prelude::set_timeout(
                    move || {
                        if !mounted.load(Ordering::Relaxed) {
                            return;
                        }
                        pending.set(false);
                        let h = elem_for_timeout.get_bounding_client_rect().height();
                        // Inactive tabs in the LRU hot set stay mounted under
                        // `display: none`, where every element measures as 0px.
                        // If we recorded those zeros, switching back to the
                        // hidden tab would compute spacers against rows the
                        // window math thinks have no height — collapsing the
                        // visible window onto rows that are actually below the
                        // viewport. Ignore zero/negative measurements; the next
                        // observer fire after the tab is shown again will
                        // record the real height.
                        if h <= 0.0 || h.is_nan() {
                            return;
                        }
                        let changed = row_heights.with_value(|map| {
                            let prev = map.get(&row_id).copied();
                            prev.is_none_or(|p| (p - h).abs() >= 0.5)
                        });
                        if changed {
                            row_heights.update_value(|map| {
                                map.insert(row_id, h);
                            });
                            heights_version.update(|v| *v = v.wrapping_add(1));
                        }
                    },
                    std::time::Duration::from_millis(0),
                );
            });
        if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            observer.observe(&element);
            slot.update_value(|s| *s = Some((observer, cb)));
        }
    });

    on_cleanup(move || {
        row_mounted.store(false, Ordering::Relaxed);
        slot.update_value(|s| {
            if let Some((observer, _cb)) = s.take() {
                observer.disconnect();
            }
        });
    });

    view! {
        <div class="virt-row" node_ref=node_ref>
            <ChatMessageView agent_ref=agent_ref row=row />
        </div>
    }
}

/// Cycle button for the global tool-output verbosity setting. Lives on the
/// chat header next to the backend badge. Reads and writes
/// `state.tool_output_mode` directly (frontend-local, persisted to
/// localStorage); never goes through the protocol.
#[component]
fn ToolOutputModeToggle() -> impl IntoView {
    let state = expect_context::<AppState>();
    let mode = state.tool_output_mode;

    let label = move || match mode.get() {
        ToolOutputMode::Summary => "\u{2298}",
        ToolOutputMode::Compact => "\u{25d0}",
        ToolOutputMode::Full => "\u{25c9}",
    };
    let title = move || match mode.get() {
        ToolOutputMode::Summary => "Tool output: summary (click to switch to compact)",
        ToolOutputMode::Compact => "Tool output: compact (click to switch to full)",
        ToolOutputMode::Full => "Tool output: full (click to switch to summary)",
    };

    let on_click = move |_| {
        let next = match mode.get_untracked() {
            ToolOutputMode::Summary => ToolOutputMode::Compact,
            ToolOutputMode::Compact => ToolOutputMode::Full,
            ToolOutputMode::Full => ToolOutputMode::Summary,
        };
        mode.set(next);
        persist_tool_output_mode(next);
    };

    view! {
        <button
            class="tool-output-mode-toggle"
            title=title
            on:click=on_click
        >{label}</button>
    }
}

/// "Review changes" header button. A navigation shortcut: visible whenever
/// the active agent owns a project that has uncommitted changes, it opens
/// (or focuses) the project's changed-file diff tab — the canonical
/// always-on inline review surface. Reviews are always-on and root-scoped
/// server-side, so this does not start a lifecycle; it only jumps to the
/// surface (with a legacy get-or-create fallback if no draft summary has
/// arrived yet). Disabled only while that fallback create is in flight.
#[component]
fn ReviewChangesButton() -> impl IntoView {
    let state = expect_context::<AppState>();
    let visibility_state = state.clone();
    let visible = move || {
        crate::components::review_view::active_agent_has_reviewable_changes(&visibility_state)
    };
    let pending_state = state.clone();
    let pending =
        move || crate::components::review_view::active_agent_review_create_pending(&pending_state);
    let click_state = state.clone();
    let on_click = move |_| {
        crate::components::review_view::create_review_for_active_agent(&click_state);
    };
    view! {
        <Show when=visible.clone()>
            <button
                class="chat-review-btn"
                disabled=pending.clone()
                title="Open the project's changed files to review and comment inline"
                on:click=on_click.clone()
            >
                <svg class="chat-review-btn-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor"
                     stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M3 2.5h7l3 3V13a.5.5 0 0 1-.5.5h-9.5A.5.5 0 0 1 2.5 13V3a.5.5 0 0 1 .5-.5z" />
                    <path d="M10 2.5V6h3" />
                    <path d="M5.5 9.25l1.5 1.5L11 7.5" />
                </svg>
                <span class="chat-review-btn-label">"Review changes"</span>
            </button>
        </Show>
    }
}

/// Pure logic tests that run under native `cargo test` (no DOM needed).
#[cfg(test)]
mod tests {
    use super::initial_history_floor;

    #[test]
    fn initial_history_floor_only_collapses_long_histories() {
        // Short / at-threshold histories render in full.
        assert_eq!(initial_history_floor(0), 0);
        assert_eq!(initial_history_floor(1), 0);
        assert_eq!(initial_history_floor(20), 0);
        // Just over the threshold: hide all but the last message.
        assert_eq!(initial_history_floor(21), 20);
        assert_eq!(initial_history_floor(1000), 999);
    }
}

/// Render-layer tests for `ChatView`'s keyed message list.
///
/// Asserts on what the user perceives — DOM identity across an append. The
/// keyed `<For>` over `(agent_id, idx)` should preserve existing rows when a
/// new message is appended (only the new tail row mounts), and the in-place
/// reactive lookup inside `ChatMessageView` should project tool-request
/// mutations onto an existing row without re-mounting it.
///
/// Run with: `tools/run-wasm-tests.sh wasm_tests::` (the script handles
/// chromedriver and `wasm-bindgen-cli` setup automatically — see CLAUDE.md).
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AppState, ChatMessageEntry};
    use leptos::mount::mount_to;
    use protocol::{AgentId, ChatMessage, MessageSender};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    /// Inject the production CSS into the test document so flex/scroll
    /// layout matches what the user sees. Without this, `.chat-messages`
    /// has no defined height and viewport-based windowing math runs
    /// against zero, defeating the test.
    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-chat")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-chat");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn message_rows(container: &HtmlElement) -> Vec<Element> {
        // Each rendered chat row is wrapped in a `.virt-row` by the
        // windowed list. The wrapping div is keyed by row id, so its
        // DOM identity is what survives an append — that's what the
        // identity assertions below need to look at.
        let nodes = container
            .query_selector_all(".chat-messages > .virt-row")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
            .collect()
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

    fn mk_user_msg(text: &str) -> ChatMessageEntry {
        ChatMessageEntry {
            message: ChatMessage {
                message_id: None,
                timestamp: 0,
                sender: MessageSender::User,
                content: text.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        }
    }

    /// A draft chat (no agent yet) surfaces a "Did you know?" feature tip on
    /// the welcome screen; once the draft binds to a real agent the welcome
    /// (and tip) give way to the conversation.
    #[wasm_bindgen_test]
    async fn draft_welcome_shows_did_you_know_tip_until_agent_binds() {
        ensure_styles_loaded();
        let container = make_container();
        let agent_ref: RwSignal<Option<ActiveAgentRef>> = RwSignal::new(None);
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(10_003) agent_ref=agent_ref.into() is_active=is_active_signal /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Did you know?"),
            "draft welcome must show a feature tip: {text}"
        );
        assert!(
            DID_YOU_KNOW_TIPS
                .iter()
                .any(|(title, body)| text.contains(title) && text.contains(body)),
            "tip content must come from the curated list: {text}"
        );

        agent_ref.set(Some(ActiveAgentRef {
            host_id: "host-a".to_owned(),
            agent_id: AgentId("agent-tip".to_owned()),
        }));
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Did you know?"),
            "tip must disappear once the draft binds to an agent: {text}"
        );
    }

    #[wasm_bindgen_test]
    async fn appending_a_message_preserves_existing_row_identity() {
        let agent_id = AgentId("agent-1".to_owned());
        let host_id = "host-a".to_owned();

        // Bind a separate handle to the state so we can mutate it after mount.
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            // ChatView reads its own `agent_ref` Signal prop directly; we
            // don't need to populate the global `active_agent` Memo for the
            // test to exercise the keyed-list behaviour.
            state.chat_rows.update(|m| {
                m.insert(
                    agent_id_for_mount.clone(),
                    vec![
                        ChatRowHandle::new(mk_user_msg("first")),
                        ChatRowHandle::new(mk_user_msg("second")),
                        ChatRowHandle::new(mk_user_msg("third")),
                    ],
                );
            });
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(10_001) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;

        let rows_before = message_rows(&container);
        assert_eq!(
            rows_before.len(),
            3,
            "expected 3 rendered rows pre-append, got {}",
            rows_before.len()
        );
        let row0_before: Element = rows_before[0].clone();
        let row2_before: Element = rows_before[2].clone();

        // Append a 4th message — the keyed `<For>` should add a single row at
        // the tail and leave rows 0..3 in place.
        let state = state_handle
            .borrow()
            .as_ref()
            .cloned()
            .expect("state captured");
        state.push_chat_entry(agent_id.clone(), mk_user_msg("fourth"));

        next_tick().await;

        let rows_after = message_rows(&container);
        assert_eq!(
            rows_after.len(),
            4,
            "expected 4 rendered rows post-append, got {}",
            rows_after.len()
        );

        // Row identity for the existing rows must survive — proves the keyed
        // `<For>` actually keyed (and didn't rebuild the list).
        assert!(
            row0_before.is_same_node(Some(&rows_after[0])),
            "row 0 was remounted on append — keyed <For> failed"
        );
        assert!(
            row2_before.is_same_node(Some(&rows_after[2])),
            "row 2 was remounted on append — keyed <For> failed"
        );
        // Row 3 is the freshly mounted tail.
        assert!(
            rows_after[3]
                .text_content()
                .unwrap_or_default()
                .contains("fourth"),
            "newly appended row should display the appended content"
        );
    }

    /// With a long transcript the windowed `<For>` should mount only a
    /// small fraction of the rows. Asserts on what the user *can't*
    /// perceive: rows whose offsets are far below the viewport never
    /// hit the DOM, so the bottom spacer reserves their estimated
    /// height instead. This is the load-bearing assertion for the
    /// "1600-message chats are slow" fix — if it regresses, every
    /// future signal touch on the chat will scale linearly with
    /// transcript length again.
    #[wasm_bindgen_test]
    async fn windowed_list_does_not_mount_all_rows_for_long_transcript() {
        ensure_styles_loaded();

        let agent_id = AgentId("agent-virt".to_owned());
        let host_id = "host-virt".to_owned();
        // 200 rows is well above the viewport / overscan budget at any
        // row height — this confirms windowing engaged, not just that
        // the test container happened to be too small.
        let total_rows = 200usize;

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            let rows: Vec<ChatRowHandle> = (0..total_rows)
                .map(|i| ChatRowHandle::new(mk_user_msg(&format!("msg {i}"))))
                .collect();
            state.chat_rows.update(|m| {
                m.insert(agent_id_for_mount.clone(), rows);
            });
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(10_002) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        // Second tick lets the viewport ResizeObserver and per-row
        // ResizeObservers fire so the visible-window Memo recomputes
        // against measured heights rather than the 200px estimate.
        next_tick().await;
        let scroller: HtmlElement = container
            .query_selector(".chat-messages")
            .unwrap()
            .expect("chat scroller present")
            .dyn_into()
            .unwrap();
        // Production chat views sticky-scroll to the bottom on mount when
        // the user has not explicitly scrolled up. This test is about the
        // top-window geometry, so force that scroll position before
        // asserting that the unmounted suffix is represented by the
        // bottom spacer.
        scroller.set_scroll_top(0);
        // `set_scroll_top` moves the DOM, but the windowing Memo only
        // re-anchors to the top once the production `scroll` listener
        // observes the new position (updating `scroll_top_sig`) and marks
        // `user_scrolled_up`, which stops sticky-bottom auto-scroll from
        // re-pinning to the end. The browser dispatches `scroll`
        // asynchronously; under full-suite event-loop load that dispatch
        // can land after the assertion, leaving the list bottom-anchored
        // (bottom spacer 0px). Dispatch it synchronously so the
        // scrolled-to-top precondition is deterministic — this drives the
        // exact same listener the browser would.
        scroller
            .dispatch_event(&web_sys::Event::new("scroll").unwrap())
            .unwrap();
        next_tick().await;

        let mounted = message_rows(&container);
        assert!(
            !mounted.is_empty(),
            "expected the windowed list to mount at least one row"
        );
        assert!(
            mounted.len() < total_rows,
            "windowing did not engage: mounted {} of {} rows",
            mounted.len(),
            total_rows,
        );

        // The bottom spacer should reserve nonzero height representing
        // the unmounted suffix of the transcript. If the spacer is
        // missing or zero-height, scrollbar geometry no longer
        // matches reality and the user can't scroll into the
        // unmounted rows.
        let spacer = container
            .query_selector(".virt-spacer-bottom")
            .unwrap()
            .expect("bottom spacer must be present in the DOM");
        let spacer_html: HtmlElement = spacer.dyn_into().unwrap();
        let height = spacer_html.get_bounding_client_rect().height();
        assert!(
            height > 0.0,
            "bottom spacer must reserve geometry for unmounted rows; got {height}px"
        );
    }

    /// A long restored history is windowed down to its last message behind a
    /// "Load previous conversation history" control, and revealing it brings
    /// the earlier messages back. Asserts on what the user perceives: the
    /// rendered row count and the explanatory banner text.
    #[wasm_bindgen_test]
    async fn long_history_collapses_to_last_message_with_load_previous_control() {
        ensure_styles_loaded();

        let agent_id = AgentId("agent-collapse".to_owned());
        let host_id = "host-collapse".to_owned();
        let total = 25usize; // > HISTORY_COLLAPSE_THRESHOLD

        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            let rows: Vec<ChatRowHandle> = (0..total)
                .map(|i| ChatRowHandle::new(mk_user_msg(&format!("msg {i}"))))
                .collect();
            state.chat_rows.update(|m| {
                m.insert(agent_id_for_mount.clone(), rows);
            });
            state.history_floor.update(|m| {
                m.insert(agent_id_for_mount.clone(), initial_history_floor(total));
            });
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(10_007) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        next_tick().await;

        // Collapsed: only the last message is mounted; the rest hide behind
        // the load-previous control.
        let collapsed_rows = message_rows(&container);
        assert_eq!(
            collapsed_rows.len(),
            1,
            "collapsed history should render exactly the last message, got {}",
            collapsed_rows.len()
        );
        assert!(
            collapsed_rows[0]
                .text_content()
                .unwrap_or_default()
                .contains("msg 24"),
            "the single visible row should be the last message"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Load previous conversation history"),
            "collapsed history must offer the load-previous control: {text}"
        );
        assert!(
            text.contains("the AI still has the full conversation in context"),
            "collapsed history must reassure the AI retains full context: {text}"
        );
        // The control is a real button carrying that label.
        let buttons = container.query_selector_all("button").unwrap();
        let has_load_button = (0..buttons.length()).any(|i| {
            buttons
                .item(i)
                .and_then(|node| node.text_content())
                .is_some_and(|label| label.contains("Load previous conversation history"))
        });
        assert!(has_load_button, "load-previous control must be a button");

        // Reveal: drop the floor and the earlier messages come back, banner gone.
        let state = state_handle
            .borrow()
            .as_ref()
            .cloned()
            .expect("state captured");
        state.history_floor.update(|m| {
            m.insert(agent_id.clone(), 0);
        });

        next_tick().await;
        next_tick().await;

        let revealed_rows = message_rows(&container);
        assert!(
            revealed_rows.len() > 1,
            "revealing history should mount more than the single collapsed row, got {}",
            revealed_rows.len()
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Load previous conversation history"),
            "revealed history must not still show the load-previous control: {text}"
        );

        // Tear the view down inside this test: unmount (runs ChatView's
        // `on_cleanup`, disconnecting its ResizeObservers and clearing the
        // `view_mounted` flag), detach the container, and flush a tick so any
        // `set_timeout` ChatView scheduled fires against the now-unmounted view
        // instead of leaking into a later test in the shared wasm instance.
        drop(handle);
        container.remove();
        next_tick().await;
    }

    #[wasm_bindgen_test]
    async fn remount_restores_saved_scroll_position() {
        ensure_styles_loaded();

        let agent_id = AgentId("agent-scroll".to_owned());
        let host_id = "host-scroll".to_owned();
        let tab_id = TabId(10_003);
        let saved_scroll_top = 1_800;

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            let rows: Vec<ChatRowHandle> = (0..80)
                .map(|i| ChatRowHandle::new(mk_user_msg(&format!("scroll msg {i}"))))
                .collect();
            state.chat_rows.update(|m| {
                m.insert(agent_id_for_mount.clone(), rows);
            });
            state.save_tab_scroll_state(
                tab_id,
                TabScrollState {
                    scroll_top: saved_scroll_top,
                    scroll_height: 16_000,
                    client_height: 600,
                    user_scrolled_up: true,
                },
            );
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=tab_id agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        next_tick().await;

        let scroller: HtmlElement = container
            .query_selector(".chat-messages")
            .unwrap()
            .expect("chat scroller present")
            .dyn_into()
            .unwrap();
        let restored = scroller.scroll_top();
        assert!(
            restored >= saved_scroll_top - 20,
            "expected remount to restore scrollTop near {saved_scroll_top}, got {restored}"
        );
        let distance_from_bottom =
            scroller.scroll_height() - scroller.scroll_top() - scroller.client_height();
        assert!(
            distance_from_bottom > 500,
            "restored user-scrolled tab should not auto-scroll back to bottom"
        );
    }

    #[wasm_bindgen_test]
    async fn chat_view_does_not_mount_team_roster_sidebar_for_manager_chat() {
        use crate::state::AgentInfo;
        use protocol::{
            AgentControlStatus, AgentOrigin, BackendKind, CustomAgentId, StreamPath, Team, TeamId,
            TeamMember, TeamMemberBindingPayload, TeamMemberId, TeamMemberRole, TeamMemberState,
        };

        let host_id = "host-a".to_owned();
        let agent_id = AgentId("agent-mgr".to_owned());
        let manager_id = TeamMemberId("m-1".to_owned());
        let report_id = TeamMemberId("m-2".to_owned());

        let team = Team {
            id: TeamId("t-1".to_owned()),
            name: "Alpha".to_owned(),
            manager_member_id: manager_id.clone(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let make_member = |id: &TeamMemberId, name: &str, role: TeamMemberRole| TeamMember {
            id: id.clone(),
            team_id: TeamId("t-1".to_owned()),
            role,
            state: TeamMemberState::Active,
            name: name.to_owned(),
            description: String::new(),
            profile: None,
            custom_agent_id: None,
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            session_id: None,
            project_ids: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let manager_member = make_member(&manager_id, "Manager A", TeamMemberRole::Manager);
        let report_member = make_member(&report_id, "Report A", TeamMemberRole::Report);

        let container = make_container();
        let host_for_mount = host_id.clone();
        let agent_id_for_mount = agent_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.update(|agents| {
                agents.push(AgentInfo {
                    host_id: host_for_mount.clone(),
                    agent_id: agent_id_for_mount.clone(),
                    name: "Manager A".to_owned(),
                    origin: AgentOrigin::TeamMember,
                    backend_kind: BackendKind::Claude,
                    workspace_roots: vec!["/repo".to_owned()],
                    project_id: None,
                    parent_agent_id: None,
                    session_id: None,
                    custom_agent_id: Some(CustomAgentId("ca-1".to_owned())),
                    workflow: None,
                    created_at_ms: 0,
                    instance_stream: StreamPath("/agent/agent-mgr".to_owned()),
                    started: true,
                    fatal_error: None,
                    activity_summary: Default::default(),
                });
            });
            state.teams.update(|m| {
                m.entry(host_for_mount.clone())
                    .or_default()
                    .insert(team.id.clone(), team.clone());
            });
            state.team_members.update(|m| {
                let host_map = m.entry(host_for_mount.clone()).or_default();
                host_map.insert(manager_member.id.clone(), manager_member.clone());
                host_map.insert(report_member.id.clone(), report_member.clone());
            });
            state.team_member_bindings.update(|m| {
                m.entry(host_for_mount.clone()).or_default().insert(
                    manager_id.clone(),
                    TeamMemberBindingPayload {
                        member_id: manager_id.clone(),
                        current_agent_id: Some(agent_id_for_mount.clone()),
                        status: AgentControlStatus::Idle,
                        last_active_at_ms: Some(42),
                    },
                );
            });

            provide_context(state.clone());
            let bound = ActiveAgentRef {
                host_id: host_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_001) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        next_tick().await;

        assert!(
            container
                .query_selector(".team-roster-sidebar")
                .unwrap()
                .is_none(),
            "chat view should not mount the old team roster sidebar"
        );
    }

    #[wasm_bindgen_test]
    async fn chat_view_does_not_mount_team_roster_sidebar_for_draft_team_member_tab() {
        use crate::state::TabContent;
        use protocol::TeamMemberId;

        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.open_tab(
                TabContent::team_member_draft(
                    "host-draft".to_owned(),
                    TeamMemberId("m-draft-mgr".to_owned()),
                ),
                "Draft Manager".to_owned(),
                true,
            );
            provide_context(state);
            let agent_ref_signal: Signal<Option<ActiveAgentRef>> = Signal::derive(|| None);
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_002) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        next_tick().await;

        assert!(
            container
                .query_selector(".team-roster-sidebar")
                .unwrap()
                .is_none(),
            "draft team-member chat should not mount the old team roster sidebar"
        );
    }
}
