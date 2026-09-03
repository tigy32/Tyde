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
use crate::components::inflight_tray::InflightTray;
use crate::components::orchestration_view::OrchestrationView;
use crate::components::settings_panel::persist_tool_output_mode;
use crate::components::task_list::TaskListView;
use crate::send::send_frame;
use crate::state::{
    ActiveAgentRef, AgentInfo, AppState, ChatNotice, ChatRowContent, ChatRowHandle, ChatRowId,
    ContextCompactionUiState, PendingHistoryRequest, TabId, TabScrollState, ToolOutputMode,
};

use protocol::{
    BackendKind, CompactionMethod, CompactionMutation, CompactionStage, CompactionTrigger,
    ContextCompactionStatus, ContextCompactionTimelineEvent, ContextCompactionTimelineStatus,
    FetchSessionHistoryPayload, FrameKind, HistoryPageRequestId, ProjectDiffScope,
    ReviewCreatePayload, ReviewDiffSelection, StreamPath,
};

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

const SESSION_HISTORY_PAGE_LIMIT: u32 = 50;

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
    /// True only when this chat owns the *visible* composer — the focused
    /// pane's chat, else the other pane's. Every chat now mounts its own
    /// composer, so this no longer gates the composer; it gates the controls
    /// that are client-global and must render exactly once (see
    /// `ToolOutputModeToggle`, dev-docs/32 §7).
    #[prop(optional)]
    owns_composer: Option<Signal<bool>>,
    /// Compatibility input for the pre-split center zone. Remove once every
    /// caller supplies `owns_composer` from the layout foundation.
    #[prop(optional)]
    is_active: Option<Signal<bool>>,
    /// Whether this chat is the visible tab in its pane, and so mounts a
    /// composer. Hidden tabs stay mounted to preserve scroll and find state,
    /// but a composer the user cannot see is not one they can type into.
    /// Defaults to the composer-owner signal so single-pane callers that pass
    /// only `is_active`/`owns_composer` keep their existing behaviour.
    #[prop(optional)]
    has_composer: Option<Signal<bool>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let owns_composer = owns_composer
        .or(is_active)
        .unwrap_or_else(|| Signal::derive(|| false));
    let has_composer = has_composer.unwrap_or(owns_composer);
    // This pane's own composer and its own pending team member. Both are keyed
    // to `tab_id` rather than to the composer owner, which is what keeps one
    // pane's draft and spawn choices out of the other's.
    let composer = state.composer_for(tab_id);
    let pending_state = state.clone();
    let composer_pending_team_member =
        Signal::derive(move || pending_state.tab_pending_team_member(tab_id));
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

    let prior_history: Signal<Option<crate::state::SessionHistoryState>> =
        Signal::derive(move || {
            let id = active_agent_id()?;
            state.session_history.with(|m| m.get(&id).cloned())
        });

    let state_for_history_load = state.clone();
    let load_prior_history = Callback::new(move |_: web_sys::MouseEvent| {
        let state = state_for_history_load.clone();
        let Some(agent_ref) = agent_ref.get_untracked() else {
            return;
        };
        let Some(agent) = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|agent| {
                    agent.host_id == agent_ref.host_id && agent.agent_id == agent_ref.agent_id
                })
                .cloned()
        }) else {
            log::error!(
                "load_prior_history: active agent stream missing for host={} agent={}",
                agent_ref.host_id,
                agent_ref.agent_id
            );
            return;
        };
        let Some(history) = state
            .session_history
            .with_untracked(|m| m.get(&agent_ref.agent_id).cloned())
        else {
            return;
        };
        if history.loading() {
            return;
        }
        // Stamp the request so the response can be correlated. Without this the
        // client can only know that *a* fetch is out, and a page from a
        // previous connection lands in a transcript it does not belong to.
        let request = PendingHistoryRequest {
            request_id: HistoryPageRequestId(crate::state::new_history_request_id()),
            before_seq: history.oldest_seq,
        };
        state.session_history.update(|map| {
            if let Some(history) = map.get_mut(&agent_ref.agent_id) {
                history.pending_request = Some(request.clone());
            }
        });
        let state_for_error = state.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let host_id = agent.host_id.clone();
            let stream = agent.instance_stream.clone();
            let payload = FetchSessionHistoryPayload {
                agent_id: agent.agent_id.clone(),
                request_id: request.request_id.clone(),
                before_seq: request.before_seq,
                limit: SESSION_HISTORY_PAGE_LIMIT,
            };
            if let Err(error) =
                send_frame(&host_id, stream, FrameKind::FetchSessionHistory, &payload).await
            {
                log::error!("failed to send fetch_session_history: {error}");
                state_for_error.session_history.update(|map| {
                    if let Some(history) = map.get_mut(&payload.agent_id) {
                        // Only clear if this is still *our* request; a newer
                        // one may have replaced it.
                        if history.pending_request.as_ref() == Some(&request) {
                            history.pending_request = None;
                        }
                    }
                });
            }
        });
    });

    // `.with` reads through the HashMap signals without cloning the
    // entire map — the previous `.get()` allocated a fresh
    // HashMap<AgentId, StreamingState> on every read, and these
    // closures fire from the auto-scroll Effect on every stream-start
    // / stream-end, plus per-render in the streaming-card branch.
    let streaming = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.streaming_text.with(|m| m.get(&agent_id).cloned())
    };

    let task_list: Signal<Option<protocol::TaskList>> = Signal::derive(move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.task_lists.with(|m| m.get(&agent_id).cloned())
    });

    let orchestration_records: Signal<Vec<crate::state::OrchestrationRecord>> =
        Signal::derive(move || {
            let Some(agent_id) = agent_ref.get().map(|agent| agent.agent_id) else {
                return Vec::new();
            };
            state
                .orchestration
                .with(|m| m.get(&agent_id).cloned().unwrap_or_default())
        });

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

    let context_breakdown: Memo<Option<protocol::ContextBreakdown>> = Memo::new(move |_| {
        let active = agent_ref.get()?;
        if current_agent
            .get()
            .is_some_and(|agent| agent.backend_kind == BackendKind::Codex)
        {
            return state.agent_activity_stats.with(|stats| {
                stats
                    .get(&crate::state::ActiveAgentRef {
                        host_id: active.host_id,
                        agent_id: active.agent_id,
                    })
                    .and_then(|stats| stats.estimated_context_breakdown.clone())
            });
        }

        state.chat_rows.with(|rows_by_agent| {
            let rows = rows_by_agent.get(&active.agent_id)?;
            for row in rows.iter().rev() {
                let Some(row_entry) = row.message_entry() else {
                    continue;
                };
                let entry = row_entry.get();
                if !matches!(
                    entry.message.sender,
                    protocol::MessageSender::Assistant { .. }
                ) {
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

    // Has this session ever reported its context occupancy? A session that
    // reported one and now doesn't has a *gap* worth naming: the reader had a
    // figure a moment ago and it went missing. A session that has never
    // reported one has nothing to be unavailable about, and must not grow a
    // context panel whose only content would be the word "Unavailable".
    let context_was_ever_reported = Memo::new(move |_| {
        let Some(active) = agent_ref.get() else {
            return false;
        };
        state.chat_rows.with(|rows_by_agent| {
            rows_by_agent.get(&active.agent_id).is_some_and(|rows| {
                rows.iter().any(|row| {
                    row.message_entry()
                        .is_some_and(|entry| entry.get().message.context_breakdown.is_some())
                })
            })
        })
    });

    let current_context_usage: Signal<Option<protocol::CurrentContextUsage>> =
        Signal::derive(move || {
            let active = agent_ref.get()?;
            // Occupancy reported out-of-band, on the agent's activity stats
            // rather than on a message. Keyed on the data being there and not
            // on which backend produced it: Codex was simply the first to
            // report this way, and hardcoding it meant a second backend that
            // did the same was ignored and rendered no context panel at all.
            let reported_out_of_band = state.agent_activity_stats.with(|stats| {
                stats
                    .get(&crate::state::ActiveAgentRef {
                        host_id: active.host_id,
                        agent_id: active.agent_id,
                    })
                    .and_then(|stats| stats.current_context_usage.clone())
            });
            if let Some(usage) = reported_out_of_band {
                return Some(usage);
            }
            if current_agent
                .get()
                .is_some_and(|agent| agent.backend_kind == BackendKind::Codex)
            {
                return Some(protocol::CurrentContextUsage::Unknown);
            }

            // Backends that report occupancy per message have ordinary gaps —
            // an interrupted turn, a phase emitted without usage. Report the
            // gap as `Unknown`, the same as Codex does, rather than as "this
            // agent has no context": the latter unmounts the whole summary
            // panel, taking the control that switches back to it along with
            // it. A session that never reported occupancy has no gap to
            // report, so it stays silent.
            if context_breakdown.get().is_none() && context_was_ever_reported.get() {
                return Some(protocol::CurrentContextUsage::Unknown);
            }
            None
        });

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
        let saved = initial_scroll_state;
        if saved.is_none() {
            return;
        }
        restored_initial_scroll_for_effect.set(true);
        let restore_user_scrolled_up = saved.is_some_and(|scroll| scroll.user_scrolled_up);
        let target_scroll_top = if restore_user_scrolled_up {
            saved.map(|scroll| scroll.scroll_top).unwrap_or(0)
        } else {
            el.scroll_height()
        };
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
        let rows = row_handles();
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
                            BackendKind::Kiro => ("backend-badge acp", "Kiro"),
                            BackendKind::Claude => ("backend-badge claude", "Claude"),
                            BackendKind::Codex => ("backend-badge codex", "Codex"),
                            BackendKind::Antigravity => ("backend-badge antigravity", "Antigravity"),
                            BackendKind::Hermes => ("backend-badge hermes", "Hermes"),
                            BackendKind::Grok => ("backend-badge grok", "Grok"),
                            BackendKind::Opencode => ("backend-badge opencode", "OpenCode"),
                        };
                        view! { <span class=badge_class>{label}</span> }
                    })}
                    <Show when=move || owns_composer.get()>
                        <ToolOutputModeToggle />
                    </Show>
                    <ReviewChangesButton agent_ref=agent_ref />
                    <CompactContextButton agent_ref=agent_ref />
                </div>
                <TaskListView
                    agent_id=Signal::derive(move || {
                        agent_ref.get().map(|active| active.agent_id)
                    })
                    task_list=task_list
                    context_breakdown=context_breakdown
                    current_context_usage=current_context_usage
                />
                <Show when=agent_initializing>
                    <div class="chat-initializing-overlay">
                        <div class="chat-initializing-spinner"></div>
                        <p class="chat-initializing-text">"Initializing agent\u{2026}"</p>
                    </div>
                </Show>
                <div class="chat-messages-wrapper">
                    <div class="chat-messages" node_ref=scroll_ref>
                        {move || {
                            if !has_messages()
                                && streaming().is_none()
                                && prior_history.get().is_none()
                                && !agent_initializing()
                            {
                                Some(view! {
                                    <div class="chat-empty-hint">
                                        <p>"Type a message to start the conversation"</p>
                                    </div>
                                })
                            } else {
                                None
                            }
                        }}

                        <Show when=move || prior_history.get().is_some()>
                            <div class="chat-history-collapsed">
                                <button
                                    class="chat-history-load-previous"
                                    disabled=move || prior_history.get().is_some_and(|history| history.loading())
                                    on:click={
                                        let load_prior_history = load_prior_history;
                                        move |event| load_prior_history.run(event)
                                    }
                                >
                                    {move || {
                                        let Some(history) = prior_history.get() else {
                                            return String::new();
                                        };
                                        if history.loading() {
                                            return "Loading earlier messages…".to_owned();
                                        }
                                        if history.message_count > 0 {
                                            if history.message_count == 1 {
                                                "Load earlier messages (1 message)".to_owned()
                                            } else {
                                                format!(
                                                    "Load earlier messages ({} messages)",
                                                    history.message_count
                                                )
                                            }
                                        } else {
                                            "Load earlier messages".to_owned()
                                        }
                                    }}
                                </button>
                                <p class="chat-history-collapsed-note">
                                    "Earlier messages are available on demand."
                                </p>
                            </div>
                        </Show>

                        // Windowed rows: top spacer + visible rows +
                        // bottom spacer. The spacers reserve scroll
                        // geometry for the unrendered rows so the
                        // scrollbar tracks total estimated height even
                        // though we only mount what's near the viewport.
                        // `MeasuredRow` reports each rendered row's
                        // post-layout height back into `row_heights`,
                        // which keeps the spacers honest as the user
                        // scrolls into previously-unmeasured regions.
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
                                let rows = row_handles();
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

                        // Live compaction state. Outside the windowed list, so
                        // it stays visible wherever the user has scrolled and
                        // disappears the moment the operation ends.
                        <ContextCompactionBanner agent_ref=agent_ref />

                        <OrchestrationView records=orchestration_records />

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
            // The In-flight tray sits between the transcript and the
            // composer: the single live surface for this chat's background
            // work (child agents, sub-agents, workflows) and its queued
            // messages. It renders nothing when the agent has nothing in
            // flight, and is deliberately independent of the tool-output
            // mode — live operational state is not transcript history.
            <InflightTray agent_ref=agent_ref />
            // Every visible chat mounts its own composer. A chat beside another
            // chat is directly repliable; neither pane has to be focused first,
            // and neither pane's draft, backend choice, or session settings can
            // reach the other (dev-docs/32 §7).
            <Show when=move || has_composer.get()>
                <ChatInput
                    agent_ref=agent_ref
                    pending_team_member=composer_pending_team_member
                    composer=composer.clone()
                />
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
            {match row.content {
                ChatRowContent::Message(entry) => {
                    view! { <ChatMessageView agent_ref=agent_ref entry=entry /> }.into_any()
                }
                ChatRowContent::ContextCompaction(event) => {
                    view! { <ContextCompactionMarker event=event /> }.into_any()
                }
                ChatRowContent::Notice(notice) => {
                    view! { <ChatNoticeView notice=notice /> }.into_any()
                }
            }}
        </div>
    }
}

// ── Context compaction ──────────────────────────────────────────────────

/// The chat header's compaction control. Discoverability: the agent-card
/// action is hover-revealed in a side panel, which is not where a user
/// working in a long conversation is looking when they need it.
///
/// Visible-but-disabled with a reason, never hidden — and enabled during a
/// turn, because the server defers rather than refuses.
#[component]
fn CompactContextButton(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();

    let control = Signal::derive(move || {
        let agent = agent_ref.get()?;
        Some(crate::actions::compaction_control_state(&state, &agent))
    });

    let on_click = move |_: web_sys::MouseEvent| {
        let Some(agent) = agent_ref.get_untracked() else {
            return;
        };
        let state: AppState = expect_context::<AppState>();
        let name = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|candidate| {
                    candidate.host_id == agent.host_id && candidate.agent_id == agent.agent_id
                })
                .map(|candidate| candidate.name.clone())
                .unwrap_or_else(|| "this agent".to_owned())
        });
        wasm_bindgen_futures::spawn_local(async move {
            crate::actions::request_context_compaction(state, agent, name).await;
        });
    };

    view! {
        {move || {
            let control = control.get()?;
            let enabled = control.is_enabled();
            let label = match control.reason() {
                None => "Compact context".to_owned(),
                Some(reason) => format!("Compact context — unavailable: {reason}"),
            };
            Some(view! {
                <button
                    type="button"
                    class="chat-header-compact"
                    title=label.clone()
                    aria-label=label
                    // `aria-disabled`, not the native attribute: a natively
                    // disabled button leaves the tab order and its reason
                    // becomes hover-only. `request_context_compaction`
                    // re-checks the gate, so the click is inert regardless.
                    aria-disabled=move || if enabled { "false" } else { "true" }
                    data-test="chat-header-compact"
                    on:click=on_click
                >
                    "\u{27F2}"
                </button>
            })
        }}
    }
}

/// `384168` → `"384.2K"`. Visible text only; the accessible sentence spells
/// the number out (see `dispatch::compaction_marker_announcement`).
fn compaction_token_text(tokens: u64) -> String {
    crate::components::chat_message::format_compact(tokens)
}

/// Elapsed milliseconds to whole seconds, rounded to nearest.
///
/// Truncating understates every duration by up to a second, so the real
/// corpus figure 169_775 ms rendered as `2m 49s` rather than the `2m 50s` it
/// actually is. The visible row and the accessible sentence must round the
/// same way or they disagree about the same event.
pub(crate) fn compaction_duration_seconds(duration_ms: u64) -> u64 {
    duration_ms.saturating_add(500) / 1000
}

/// `169775` → `"2m 50s"`, `48200` → `"48s"`.
fn compaction_duration_text(duration_ms: u64) -> String {
    let seconds = compaction_duration_seconds(duration_ms);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let rest = seconds % 60;
    if rest == 0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m {rest}s")
    }
}

/// Which side did the work. Shown only when it disambiguates something the
/// user could act on — "Tyde summarized this itself" is materially different
/// from "the backend compacted its own context".
fn compaction_method_text(method: CompactionMethod) -> Option<&'static str> {
    match method {
        CompactionMethod::NativeTextCommand | CompactionMethod::NativeRpc => None,
        CompactionMethod::InlineFallback => Some("Tyde fallback"),
        CompactionMethod::BackendAutomatic => None,
    }
}

fn compaction_marker_title(event: &ContextCompactionTimelineEvent) -> &'static str {
    match event.status {
        ContextCompactionTimelineStatus::Completed => match event.trigger {
            CompactionTrigger::BackendAutomatic => "Context compacted automatically",
            _ => "Context compacted",
        },
        // The failure wording is driven by *mutation*, not by the error text.
        // What the user needs first is whether their model context survived;
        // the provider's prose is secondary and is appended verbatim below.
        ContextCompactionTimelineStatus::Failed => match event.mutation {
            CompactionMutation::NotObserved => "Compaction failed — context unchanged",
            CompactionMutation::Completed => "Context compacted, but finalizing failed",
            CompactionMutation::MayHaveMutated => "Compaction failed — context may have changed",
        },
    }
}

/// The durable timeline marker.
///
/// Deliberately **not** a chat card: no sender, no timestamp, no copy control,
/// no markdown body. And deliberately **not** a live region of any kind — the
/// announcement for a compaction is made once, by the reducer, at the moment
/// the event arrives live (see `dispatch::apply_chat_event_from` and
/// `apply_context_compaction_notify`). This row is history; it is re-mounted
/// on every scroll pass, page-back, and reconnect.
#[component]
fn ContextCompactionMarker(event: ArcRwSignal<ContextCompactionTimelineEvent>) -> impl IntoView {
    // Read reactively: a later, richer sighting of the same marker writes
    // through this signal rather than appending a row.
    view! {
        {move || {
            let event = event.get();
            context_compaction_marker_view(&event)
        }}
    }
}

fn context_compaction_marker_view(event: &ContextCompactionTimelineEvent) -> impl IntoView + use<> {
    let title = compaction_marker_title(event);
    let failed = matches!(event.status, ContextCompactionTimelineStatus::Failed);

    // Every metric is optional and absence is normal — backends differ in what
    // they report. A missing figure renders as *nothing*, never as a dash, a
    // zero, or "unknown": a fabricated 0 reads as "compacted to nothing".
    let tokens = match (event.metrics.before_tokens, event.metrics.after_tokens) {
        (Some(before), Some(after)) => Some(format!(
            "{} → {}",
            compaction_token_text(before),
            compaction_token_text(after)
        )),
        (Some(before), None) => Some(format!("from {}", compaction_token_text(before))),
        (None, Some(after)) => Some(format!("to {}", compaction_token_text(after))),
        (None, None) => None,
    };
    let duration = event.metrics.duration_ms.map(compaction_duration_text);
    let method = compaction_method_text(event.method).map(str::to_owned);
    let reason = event
        .message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned);

    // Exactly one representation in the accessibility tree.
    //
    // The visible row abbreviates for scanning (`384.2K`, `2m 50s`); read
    // aloud those are ambiguous or spelled as letters. So the whole visible row
    // is `aria-hidden`, and a single visually-hidden sentence carries the same
    // facts in full — grouped digits, humanized duration, and the guarantee
    // about the transcript. Naming the row with `aria-label` instead would
    // leave the abbreviated descendants in the tree alongside it, which is the
    // duplicate-metric shape the contract forbids.
    //
    // No `role`: V1 is a plain historical element (plan §11). Not
    // `separator` (risks suppressing its content), not `group` (no
    // assistive-technology evidence yet), and above all not `status` or
    // `alert` — the virtualizer mounts and unmounts this row as the user
    // scrolls, and bootstrap and paging re-mount it wholesale, so live-region
    // semantics here would replay an old outcome on every pass.
    let accessible_sentence = crate::dispatch::compaction_marker_announcement(event);

    view! {
        <div
            class=move || {
                if failed {
                    "context-compaction-marker context-compaction-marker-failed"
                } else {
                    "context-compaction-marker"
                }
            }
            data-test="context-compaction-marker"
        >
            <span class="visually-hidden">{accessible_sentence}</span>
            <span class="context-compaction-visual" aria-hidden="true">
                <span class="context-compaction-rule"></span>
                <span class="context-compaction-label">
                    <span class="context-compaction-icon">"\u{27F2}"</span>
                    <span class="context-compaction-title">{title}</span>
                    {tokens.map(|tokens| view! {
                        <span class="context-compaction-metric">{tokens}</span>
                    })}
                    {duration.map(|duration| view! {
                        <span class="context-compaction-metric">{duration}</span>
                    })}
                    {method.map(|method| view! {
                        <span class="context-compaction-method">{method}</span>
                    })}
                    {reason.map(|reason| view! {
                        <span class="context-compaction-reason">{reason}</span>
                    })}
                </span>
                <span class="context-compaction-rule"></span>
            </span>
        </div>
    }
}

/// A retry or cancellation, rendered where it happened.
///
/// A row, not a floating card, for the same reason the compaction marker is
/// (see [`ChatRowContent`]): it describes one specific turn, so it has to keep
/// that turn's position when later turns arrive. It is also not a live region
/// — the virtualizer mounts and unmounts it on every scroll pass, and bootstrap
/// re-mounts it wholesale, so live-region semantics would replay an old
/// rate-limit warning as news.
#[component]
fn ChatNoticeView(notice: ArcRwSignal<ChatNotice>) -> impl IntoView {
    view! {
        {move || match notice.get() {
            ChatNotice::OperationCancelled { message } => view! {
                <div class="chat-card chat-card-system chat-card-cancelled" data-test="chat-notice-cancelled">
                    <div class="chat-card-header">
                        <span class="chat-card-sender">"Cancelled"</span>
                    </div>
                    <div class="chat-card-body">
                        <p class="md-paragraph">{message}</p>
                    </div>
                </div>
            }.into_any(),
            ChatNotice::RetryAttempt { attempt, max_retries, error, backoff_ms } => view! {
                <div class="chat-card chat-card-retry" data-test="chat-notice-retry">
                    <div class="retry-card-header">
                        <span class="retry-card-icon">"\u{23f3}"</span>
                        <span class="retry-card-title">"Rate Limited"</span>
                        <span class="retry-card-attempt">
                            {format!("Attempt {attempt} of {max_retries}")}
                        </span>
                    </div>
                    <div class="retry-card-body">
                        <p class="retry-card-error">{error}</p>
                        <p class="retry-card-countdown">
                            {format!("Retried after {backoff_ms}ms")}
                        </p>
                    </div>
                </div>
            }.into_any(),
        }}
    }
}

fn compaction_stage_text(stage: CompactionStage) -> &'static str {
    match stage {
        CompactionStage::WaitingForIdle => "Waiting for the current turn to finish.",
        CompactionStage::Dispatching => "Starting.",
        CompactionStage::Compacting => "Summarizing the conversation for the model.",
        CompactionStage::Finalizing => "Finalizing.",
    }
}

/// The live operation banner.
///
/// Operational state, not transcript history: it sits outside the virtualized
/// list so it stays visible wherever the user has scrolled, and it disappears
/// the moment the operation ends.
///
/// Sitting outside the list is what makes "in flight only" load-bearing rather
/// than cosmetic. A card with no row has no position in the conversation, so
/// anything retained here after the operation ends stays welded to the tip
/// while later turns render above it. The outcome belongs to the durable marker
/// row, which is anchored where the compaction actually happened.
///
/// Live-region behaviour is deliberately narrow. Inserting a node into an
/// `aria-live` region *is* an announcement, so a banner reconstructed from
/// `AgentBootstrap` renders with `aria-live="off"` — visible, but silent —
/// until a genuinely live frame updates it. It is never an `alert`: the single
/// assertive announcement for a terminal outcome is made once, at the live
/// transition, through the shared live region, so a remount or a route change
/// cannot replay it.
#[component]
fn ContextCompactionBanner(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();

    let operation = Signal::derive(move || {
        let agent_id = agent_ref.get()?.agent_id;
        state
            .context_compactions
            .with(|map| map.get(&agent_id).cloned())
    });

    // Elapsed wall-clock for a running operation. Claude's manual compaction
    // ran 108–189 s in the corpus, and a progress surface with no moving part
    // over three minutes reads as a hang.
    //
    // Keyed on operation *identity*, not on the payload: a progress heartbeat
    // arrives roughly every 30 s and changes the payload, so an effect that
    // read the whole operation would reset the count to zero twice a minute
    // and the timer would never pass 0:30.
    let operation_key: Memo<Option<(String, bool)>> = Memo::new(move |_| {
        let agent_id = agent_ref.get()?.agent_id;
        state.context_compactions.with(|map| {
            map.get(&agent_id).map(|operation| {
                (
                    operation
                        .operation_id()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                    operation.is_in_flight(),
                )
            })
        })
    });

    let elapsed = RwSignal::new(0u32);
    let ticker: StoredValue<Option<leptos::prelude::IntervalHandle>> = StoredValue::new(None);
    let clear_ticker = move || {
        ticker.update_value(|slot| {
            if let Some(handle) = slot.take() {
                handle.clear();
            }
        });
    };
    Effect::new(move |_| {
        // Clearing first makes the lifecycle explicit rather than relying on
        // where an `on_cleanup` registered inside an effect body attaches.
        clear_ticker();
        elapsed.set(0);
        let running = operation_key.get().is_some_and(|(_, in_flight)| in_flight);
        if !running {
            return;
        }
        let handle = leptos::prelude::set_interval_with_handle(
            move || {
                elapsed.update(|value| *value = value.saturating_add(1));
            },
            std::time::Duration::from_secs(1),
        )
        .ok();
        ticker.set_value(handle);
    });
    on_cleanup(clear_ticker);

    view! {
        {move || {
            let operation = operation.get()?;
            let (title, detail) = match &operation {
                ContextCompactionUiState::Requesting => (
                    "Compacting context\u{2026}".to_owned(),
                    "Requesting.".to_owned(),
                ),
                ContextCompactionUiState::Active { payload, .. } => {
                    let detail = match &payload.status {
                        ContextCompactionStatus::Deferred { stage }
                        | ContextCompactionStatus::Started { stage }
                        | ContextCompactionStatus::Progress { stage } => {
                            payload
                                .message
                                .clone()
                                .unwrap_or_else(|| compaction_stage_text(*stage).to_owned())
                        }
                        // Terminal payloads never live in `Active`, but a
                        // future status must not render a blank banner.
                        ContextCompactionStatus::Completed
                        | ContextCompactionStatus::Failed { .. } => String::new(),
                    };
                    let title = if matches!(
                        payload.status,
                        ContextCompactionStatus::Deferred { .. }
                    ) {
                        "Compaction queued".to_owned()
                    } else {
                        "Compacting context\u{2026}".to_owned()
                    };
                    (title, detail)
                }
            };

            let announces = operation.announces();
            let in_flight = operation.is_in_flight();
            Some(view! {
                <div
                    class="chat-card chat-card-compacting"
                    // Always `status`, never `alert`. See the doc comment: the
                    // one assertive announcement is made by the reducer at the
                    // live transition, not by this element, so a remount cannot
                    // re-announce.
                    role="status"
                    // Silent when reconstructed from bootstrap; polite once
                    // genuinely live.
                    aria-live=move || if announces { "polite" } else { "off" }
                    // Atomic because the parts change independently: a stage
                    // change alone ("Finalizing.") is meaningless read on its own.
                    aria-atomic="true"
                    data-test="context-compaction-banner"
                >
                    <div class="compacting-card-header">
                        <span class="compacting-card-icon" aria-hidden="true">"\u{27F2}"</span>
                        <span class="compacting-card-title">{title}</span>
                        // Hidden from assistive technology: inside an atomic
                        // live region a per-second counter would re-announce
                        // the whole banner every tick.
                        {in_flight.then(|| view! {
                            <span class="compacting-card-elapsed" aria-hidden="true">
                                {move || compaction_elapsed_text(elapsed.get())}
                            </span>
                        })}
                    </div>
                    <div class="compacting-card-body">
                        <p class="compacting-card-detail">{detail}</p>
                        // The guarantee the user is most likely to doubt while
                        // watching a multi-minute operation rewrite "their"
                        // conversation.
                        <p class="compacting-card-note">
                            "Your conversation history here is unchanged."
                        </p>
                    </div>
                </div>
            })
        }}
    }
}

/// `110` → `"1:50"`. Clock form, because it is read as elapsed time rather
/// than scanned as a metric.
fn compaction_elapsed_text(seconds: u32) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
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

fn agent_project_id(
    state: &AppState,
    agent_ref: &ActiveAgentRef,
    tracked: bool,
) -> Option<protocol::ProjectId> {
    let find = |agents: &[AgentInfo]| {
        agents
            .iter()
            .find(|agent| {
                agent.host_id == agent_ref.host_id && agent.agent_id == agent_ref.agent_id
            })
            .and_then(|agent| agent.project_id.clone())
    };
    if tracked {
        state.agents.with(|agents| find(agents))
    } else {
        state.agents.with_untracked(|agents| find(agents))
    }
}

fn agent_has_reviewable_changes(state: &AppState, agent_ref: &ActiveAgentRef) -> bool {
    let Some(project_id) = agent_project_id(state, agent_ref, true) else {
        return false;
    };
    state.git_status.with(|map| {
        map.get(&project_id).is_some_and(|roots| {
            roots.iter().any(|root| {
                root.files
                    .iter()
                    .any(|file| file.unstaged.is_some() || file.untracked)
            })
        })
    })
}

fn agent_review_create_pending(state: &AppState, agent_ref: &ActiveAgentRef) -> bool {
    let Some(project_id) = agent_project_id(state, agent_ref, true) else {
        return false;
    };
    state
        .review_create_pending
        .with(|map| map.contains_key(&(agent_ref.host_id.clone(), project_id)))
}

fn create_review_for_agent(state: &AppState, agent_ref: ActiveAgentRef) {
    let Some(project_id) = agent_project_id(state, &agent_ref, false) else {
        log::warn!(
            "create_review_for_agent: agent {} has no project — skipping",
            agent_ref.agent_id
        );
        return;
    };

    if !crate::components::review_view::open_changed_diff_for_project(
        state,
        &agent_ref.host_id,
        &project_id,
    ) {
        return;
    }

    let has_draft = state.review_summaries.with_untracked(|map| {
        map.get(&project_id)
            .and_then(|summaries| crate::components::review_view::pick_workspace_draft(summaries))
            .is_some()
    });
    if has_draft {
        return;
    }

    let key = (agent_ref.host_id.clone(), project_id.clone());
    let mut claimed = false;
    state.review_create_pending.update(|map| {
        let entry = map.entry(key.clone()).or_insert(0);
        if *entry == 0 {
            *entry = 1;
            claimed = true;
        }
    });
    if !claimed {
        return;
    }

    let host_id = agent_ref.host_id;
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = ReviewCreatePayload {
        request_id: None,
        selection: ReviewDiffSelection::Workspace {
            scope: ProjectDiffScope::Unstaged,
        },
    };
    let failure_state = state.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = send_frame(&host_id, stream, FrameKind::ReviewCreate, &payload).await {
            log::error!("failed to send ReviewCreate: {error}");
            failure_state.review_create_pending.update(|map| {
                if let Some(count) = map.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&key);
                    }
                }
            });
        }
    });
}

/// "Review changes" header button. A navigation shortcut: visible whenever
/// the rendered agent owns a project that has uncommitted changes, it opens
/// (or focuses) the project's changed-file diff tab — the canonical
/// always-on inline review surface. Reviews are always-on and root-scoped
/// server-side, so this does not start a lifecycle; it only jumps to the
/// surface (with a legacy get-or-create fallback if no draft summary has
/// arrived yet). Disabled only while that fallback create is in flight.
#[component]
fn ReviewChangesButton(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let visibility_state = state.clone();
    let visible = move || {
        agent_ref
            .get()
            .is_some_and(|target| agent_has_reviewable_changes(&visibility_state, &target))
    };
    let pending_state = state.clone();
    let pending = move || {
        agent_ref
            .get()
            .is_some_and(|target| agent_review_create_pending(&pending_state, &target))
    };
    let click_state = state.clone();
    let on_click = move |_| {
        if let Some(target) = agent_ref.get_untracked() {
            create_review_for_agent(&click_state, target);
        }
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
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, ChatMessageEntry, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        AgentBootstrapEvent, AgentBootstrapPayload, AgentId, AgentOrigin, BackendKind, ChatEvent,
        ChatMessage, MessageSender, ProjectGitChangeKind, ProjectGitFileStatus, ProjectId,
        ProjectRootGitStatus, ProjectRootPath, StreamPath, Task, TaskList, TaskStatus,
    };
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

    async fn next_animation_frame() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .request_animation_frame(&resolve)
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
        let tab_id = TabId(10_002);
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
            // This test owns a top-anchored viewport, so seed that user intent
            // before mounting. Waiting for sticky-bottom and then scrolling
            // raced its deferred mount callback under full-suite load.
            state.save_tab_scroll_state(
                tab_id,
                TabScrollState {
                    scroll_top: 0,
                    scroll_height: 40_000,
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
        // Second tick lets the viewport ResizeObserver and per-row
        // ResizeObservers fire so the visible-window Memo recomputes
        // against measured heights rather than the 200px estimate.
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

    /// Prior history is represented by a server-owned indicator, not by rows
    /// that the client hides after receiving them.
    #[wasm_bindgen_test]
    async fn prior_history_indicator_shows_load_control_without_rows() {
        ensure_styles_loaded();

        let agent_id = AgentId("agent-collapse".to_owned());
        let host_id = "host-collapse".to_owned();

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            state.session_history.update(|m| {
                m.insert(
                    agent_id_for_mount.clone(),
                    crate::state::SessionHistoryState {
                        message_count: 25,
                        oldest_seq: Some(42),
                        has_more_before: true,
                        pending_request: None,
                    },
                );
            });
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(10_007) agent_ref=agent_ref_signal is_active=is_active_signal /> }
        });

        next_tick().await;
        next_tick().await;

        let collapsed_rows = message_rows(&container);
        assert_eq!(
            collapsed_rows.len(),
            0,
            "prior history must not be present as hidden client rows, got {}",
            collapsed_rows.len()
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Load earlier messages (25 messages)"),
            "collapsed history must offer the load-earlier control: {text}"
        );
        assert!(
            text.contains("available on demand"),
            "history note must explain on-demand loading: {text}"
        );
        let buttons = container.query_selector_all("button").unwrap();
        let has_load_button = (0..buttons.length()).any(|i| {
            buttons
                .item(i)
                .and_then(|node| node.text_content())
                .is_some_and(|label| label.contains("Load earlier messages"))
        });
        assert!(has_load_button, "load-earlier control must be a button");

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
    async fn typing_in_multiline_composer_keeps_transcript_at_bottom() {
        ensure_styles_loaded();

        let agent_id = AgentId("agent-composer-scroll".to_owned());
        let host_id = "host-composer-scroll".to_owned();
        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![make_target_agent(
                &host_id,
                &agent_id_for_mount.0,
                None,
            )]);
            let rows: Vec<ChatRowHandle> = (0..80)
                .map(|i| ChatRowHandle::new(mk_user_msg(&format!("message {i}"))))
                .collect();
            state.chat_rows.update(|by_agent| {
                by_agent.insert(agent_id_for_mount.clone(), rows);
            });
            provide_context(state);
            let bound = ActiveAgentRef {
                host_id: host_id.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let visible: Signal<bool> = Signal::derive(|| true);
            view! {
                <ChatView
                    tab_id=TabId(10_004)
                    agent_ref=agent_ref
                    is_active=visible
                />
            }
        });
        next_tick().await;
        next_tick().await;

        let scroller: HtmlElement = container
            .query_selector(".chat-messages")
            .unwrap()
            .expect("chat scroller present")
            .dyn_into()
            .unwrap();
        let textarea: web_sys::HtmlTextAreaElement = container
            .query_selector(".chat-textarea")
            .unwrap()
            .expect("composer textarea present")
            .dyn_into()
            .unwrap();
        textarea.set_id("composer-scroll-regression-textarea");
        let input = web_sys::Event::new("input").unwrap();

        textarea.set_value("alpha\nbeta\ngamma");
        textarea.dispatch_event(&input).unwrap();
        next_animation_frame().await;
        next_tick().await;
        assert!(
            textarea.get_bounding_client_rect().height() > 50.0,
            "precondition: multiline draft must expand the composer"
        );
        assert!(
            scroller.scroll_height() > scroller.client_height(),
            "precondition: transcript must be scrollable"
        );
        scroller.set_scroll_top(scroller.scroll_height());
        let bottom_before = scroller.scroll_height() - scroller.client_height();
        assert_eq!(scroller.scroll_top(), bottom_before);

        js_sys::eval(
            r#"
            window.__composerScrollStyleHistory = [];
            window.__composerScrollObserver = new MutationObserver((records) => {
                for (const record of records) {
                    window.__composerScrollStyleHistory.push(record.oldValue || "");
                }
            });
            window.__composerScrollObserver.observe(
                document.getElementById("composer-scroll-regression-textarea"),
                { attributes: true, attributeOldValue: true, attributeFilter: ["style"] }
            );
            "#,
        )
        .unwrap();

        textarea.set_value("alpha\nbeta\ngammax");
        textarea
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_animation_frame().await;

        let distance_from_bottom =
            scroller.scroll_height() - scroller.client_height() - scroller.scroll_top();
        assert!(
            distance_from_bottom.abs() <= 1,
            "typing without changing composer height must keep the transcript at bottom; \
             it moved {distance_from_bottom}px away"
        );
        let style_history = js_sys::eval(
            r#"
            window.__composerScrollObserver.disconnect();
            JSON.stringify(window.__composerScrollStyleHistory);
            "#,
        )
        .unwrap()
        .as_string()
        .unwrap();
        assert!(
            !style_history.contains("height: auto"),
            "typing must not collapse the visible multiline composer to its minimum height; \
             observed style history: {style_history}"
        );

        drop(handle);
        container.remove();
        next_tick().await;
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
                    team_member_id: None,
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

    fn make_target_agent(host_id: &str, agent_id: &str, project_id: Option<&str>) -> AgentInfo {
        AgentInfo {
            host_id: host_id.to_owned(),
            agent_id: AgentId(agent_id.to_owned()),
            name: agent_id.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: project_id.map(|id| ProjectId(id.to_owned())),
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath(format!("/agent/{agent_id}")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    #[wasm_bindgen_test]
    async fn codex_header_uses_selected_agent_stats_without_transcript_fallback() {
        ensure_styles_loaded();
        let container = make_container();
        let selected = RwSignal::new(Some(ActiveAgentRef {
            host_id: "host-context".to_owned(),
            agent_id: AgentId("root-context".to_owned()),
        }));
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut root = make_target_agent("host-context", "root-context", None);
            root.backend_kind = BackendKind::Codex;
            let mut child = make_target_agent("host-context", "child-context", None);
            child.backend_kind = BackendKind::Codex;
            child.parent_agent_id = Some(AgentId("root-context".to_owned()));
            let mut fresh = make_target_agent("host-context", "fresh-context", None);
            fresh.backend_kind = BackendKind::Codex;
            let mut neutral = make_target_agent("host-context", "neutral-context", None);
            neutral.backend_kind = BackendKind::Codex;
            state.agents.set(vec![root, child, fresh, neutral]);

            for (agent_id, input_tokens, context_window) in
                [("root-context", 100, 1_000), ("child-context", 200, 2_000)]
            {
                state.agent_activity_stats.update(|stats| {
                    stats.insert(
                        ActiveAgentRef {
                            host_id: "host-context".to_owned(),
                            agent_id: AgentId(agent_id.to_owned()),
                        },
                        protocol::AgentActivityStats {
                            current_context_usage: Some(protocol::CurrentContextUsage::Known {
                                input_tokens,
                                context_window,
                            }),
                            estimated_context_breakdown: Some(protocol::ContextBreakdown {
                                system_prompt_bytes: 4,
                                tool_io_bytes: 8,
                                conversation_history_bytes: 12,
                                reasoning_bytes: 0,
                                context_injection_bytes: 16,
                                input_tokens,
                                context_window,
                            }),
                            ..protocol::AgentActivityStats::default()
                        },
                    );
                });
                let mut transcript = mk_user_msg("stale transcript context");
                transcript.message.sender = MessageSender::Assistant {
                    agent: "codex".to_owned(),
                };
                transcript.message.context_breakdown = Some(protocol::ContextBreakdown {
                    system_prompt_bytes: 1,
                    tool_io_bytes: 1,
                    conversation_history_bytes: 1,
                    reasoning_bytes: 1,
                    context_injection_bytes: 1,
                    input_tokens: 999,
                    context_window: 9_999,
                });
                state.chat_rows.update(|rows| {
                    rows.insert(
                        AgentId(agent_id.to_owned()),
                        vec![ChatRowHandle::new(transcript)],
                    );
                });
            }
            state.agent_activity_stats.update(|stats| {
                stats.insert(
                    ActiveAgentRef {
                        host_id: "host-context".to_owned(),
                        agent_id: AgentId("neutral-context".to_owned()),
                    },
                    protocol::AgentActivityStats {
                        current_context_usage: Some(protocol::CurrentContextUsage::Known {
                            input_tokens: 300,
                            context_window: 3_000,
                        }),
                        estimated_context_breakdown: None,
                        ..protocol::AgentActivityStats::default()
                    },
                );
            });

            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let selected_agent: Signal<Option<ActiveAgentRef>> =
                Signal::derive(move || selected.get());
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! {
                <ChatView
                    tab_id=TabId(24_001)
                    agent_ref=selected_agent
                    is_active=is_active
                />
            }
        });
        next_tick().await;

        let usage_text = || {
            container
                .query_selector("[data-testid='context-usage']")
                .unwrap()
                .and_then(|element| element.text_content())
        };
        assert_eq!(usage_text().as_deref(), Some("100 / 1.0K tokens (10.0%)"));

        selected.set(Some(ActiveAgentRef {
            host_id: "host-context".to_owned(),
            agent_id: AgentId("child-context".to_owned()),
        }));
        next_tick().await;
        assert_eq!(usage_text().as_deref(), Some("200 / 2.0K tokens (10.0%)"));

        selected.set(Some(ActiveAgentRef {
            host_id: "host-context".to_owned(),
            agent_id: AgentId("fresh-context".to_owned()),
        }));
        next_tick().await;
        assert_eq!(
            usage_text().as_deref(),
            Some("Unavailable"),
            "a selected Codex agent must keep a neutral context panel before its first request"
        );
        assert!(
            !container
                .query_selector("[data-testid='context-bar']")
                .unwrap()
                .expect("unknown context bar")
                .has_attribute("aria-valuenow"),
            "unknown occupancy must not be exposed as zero percent"
        );

        selected.set(Some(ActiveAgentRef {
            host_id: "host-context".to_owned(),
            agent_id: AgentId("neutral-context".to_owned()),
        }));
        next_tick().await;
        assert_eq!(usage_text().as_deref(), Some("300 / 3.0K tokens (10.0%)"));
        assert_eq!(
            container
                .query_selector_all("[data-testid='context-segment']")
                .unwrap()
                .length(),
            0,
            "exact occupancy without observed estimate bytes must render a neutral bar"
        );

        selected.set(Some(ActiveAgentRef {
            host_id: "host-context".to_owned(),
            agent_id: AgentId("child-context".to_owned()),
        }));
        next_tick().await;

        state_handle
            .borrow()
            .as_ref()
            .expect("mounted state")
            .agent_activity_stats
            .update(|stats| {
                stats
                    .get_mut(&ActiveAgentRef {
                        host_id: "host-context".to_owned(),
                        agent_id: AgentId("child-context".to_owned()),
                    })
                    .expect("child stats")
                    .current_context_usage = Some(protocol::CurrentContextUsage::Unknown);
            });
        next_tick().await;
        assert_eq!(
            usage_text().as_deref(),
            Some("Unavailable"),
            "post-compaction Codex context must stay visible without transcript fallback"
        );
    }

    /// dev-docs/32 §7: every chat pane mounts its own composer, and the two are
    /// independent. The client-global tool-output preference still renders
    /// exactly once, with the composer owner.
    ///
    /// This replaces an earlier assertion that two rendered chats mount exactly
    /// *one* composer plus a "Reply in this pane" button. That singleton rule
    /// was the product decision at the time; it has been deliberately reversed,
    /// so the assertion is re-pointed at the new contract rather than deleted.
    /// The guarantee it was reaching for — a chat you are not focused on is
    /// still repliable, and controls that are client-global render once — is
    /// preserved and strengthened: repliable now means *directly* repliable,
    /// and the independence of the two composers (the thing a shared composer
    /// could never give) is asserted here for the first time.
    #[wasm_bindgen_test]
    async fn split_chats_mount_independent_composers_and_one_global_tool_toggle() {
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![
                make_target_agent("host-a", "agent-a", None),
                make_target_agent("host-b", "agent-b", None),
            ]);
            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_a = Signal::derive(|| {
                Some(ActiveAgentRef {
                    host_id: "host-a".to_owned(),
                    agent_id: AgentId("agent-a".to_owned()),
                })
            });
            let agent_b = Signal::derive(|| {
                Some(ActiveAgentRef {
                    host_id: "host-b".to_owned(),
                    agent_id: AgentId("agent-b".to_owned()),
                })
            });
            let owns = Signal::derive(|| true);
            let does_not_own = Signal::derive(|| false);
            // Both panes are visible; only one owns the client-global controls.
            let visible = Signal::derive(|| true);
            view! {
                <div>
                    <ChatView
                        tab_id=TabId(30_001)
                        agent_ref=agent_a
                        owns_composer=owns
                        has_composer=visible
                    />
                    <ChatView
                        tab_id=TabId(30_002)
                        agent_ref=agent_b
                        owns_composer=does_not_own
                        has_composer=visible
                    />
                </div>
            }
        });
        next_tick().await;

        assert_eq!(
            container
                .query_selector_all(".chat-input-area")
                .unwrap()
                .length(),
            2,
            "each rendered chat must mount its own composer"
        );
        assert_eq!(
            container
                .query_selector_all(".tool-output-mode-toggle")
                .unwrap()
                .length(),
            1,
            "the client-global tool-output preference must render once"
        );
        assert_eq!(
            container
                .query_selector_all(".chat-reply-in-pane")
                .unwrap()
                .length(),
            0,
            "a directly repliable pane needs no reply-in-pane affordance"
        );

        // The point of a composer per chat: text typed in one is not visible
        // in, and cannot be sent from, the other.
        let state = state_handle.borrow().as_ref().cloned().unwrap();
        state
            .composer_for(TabId(30_001))
            .text
            .set("for agent A".to_owned());
        next_tick().await;

        let textareas = container.query_selector_all("textarea").unwrap();
        assert_eq!(textareas.length(), 2, "one textarea per mounted composer");
        let first: web_sys::HtmlTextAreaElement = textareas.item(0).unwrap().dyn_into().unwrap();
        let second: web_sys::HtmlTextAreaElement = textareas.item(1).unwrap().dyn_into().unwrap();
        assert_eq!(first.value(), "for agent A");
        assert_eq!(
            second.value(),
            "",
            "one chat's draft must never appear in the other chat's composer"
        );
        assert_eq!(
            state.composer_for(TabId(30_002)).text.get_untracked(),
            "",
            "the second chat's composer state must be untouched"
        );
    }

    #[wasm_bindgen_test]
    async fn review_button_targets_rendered_agent_not_global_active_agent() {
        let _ = js_sys::eval(
            r#"
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function() { return Promise.resolve(); };
            "#,
        );
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![
                make_target_agent("host-a", "agent-a", Some("project-a")),
                make_target_agent("host-b", "agent-b", Some("project-b")),
            ]);
            state.open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: "host-b".to_owned(),
                    agent_id: AgentId("agent-b".to_owned()),
                }),
                "Agent B".to_owned(),
                true,
            );
            state.git_status.update(|map| {
                map.insert(
                    ProjectId("project-a".to_owned()),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/repo-a".to_owned()),
                        branch: Some("main".to_owned()),
                        head_oid: None,
                        empty_tree_oid: None,
                        ahead: 0,
                        behind: 0,
                        clean: false,
                        files: vec![ProjectGitFileStatus {
                            relative_path: "src/lib.rs".to_owned(),
                            staged: None,
                            unstaged: Some(ProjectGitChangeKind::Modified),
                            untracked: false,
                        }],
                        recent_commits: Vec::new(),
                        history_has_more: false,
                    }],
                );
            });
            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let rendered_agent = Signal::derive(|| {
                Some(ActiveAgentRef {
                    host_id: "host-a".to_owned(),
                    agent_id: AgentId("agent-a".to_owned()),
                })
            });
            let owns = Signal::derive(|| false);
            view! {
                <ChatView
                    tab_id=TabId(30_003)
                    agent_ref=rendered_agent
                    owns_composer=owns
                />
            }
        });
        next_tick().await;

        let button: HtmlElement = container
            .query_selector(".chat-review-btn")
            .unwrap()
            .expect("rendered agent A has reviewable changes")
            .dyn_into()
            .unwrap();
        button.click();
        next_tick().await;

        let state = state_handle.borrow().as_ref().cloned().unwrap();
        let target = state.center_zone.with_untracked(|center| {
            center.active_tab().and_then(|tab| match &tab.content {
                TabContent::Diff {
                    host_id,
                    project_id,
                    ..
                } => Some((host_id.clone(), project_id.clone())),
                _ => None,
            })
        });
        assert_eq!(
            target,
            Some(("host-a".to_owned(), ProjectId("project-a".to_owned()))),
            "Review changes must open the rendered agent's project even while agent B is globally active"
        );
    }

    // ── Context compaction ──────────────────────────────────────────────

    fn compaction_marker_event(
        marker_id: &str,
        status: ContextCompactionTimelineStatus,
        mutation: CompactionMutation,
        metrics: protocol::CompactionMetrics,
    ) -> ContextCompactionTimelineEvent {
        ContextCompactionTimelineEvent {
            marker_id: protocol::CompactionObservationId(marker_id.to_owned()),
            operation_id: None,
            trigger: CompactionTrigger::UserRequested,
            method: CompactionMethod::NativeTextCommand,
            backend_kind: BackendKind::Claude,
            provider_session_id: None,
            status,
            mutation,
            metrics,
            message: None,
            timestamp: 0,
        }
    }

    fn full_metrics() -> protocol::CompactionMetrics {
        protocol::CompactionMetrics {
            before_tokens: Some(384_168),
            after_tokens: Some(12_518),
            duration_ms: Some(169_775),
            ..Default::default()
        }
    }

    fn mount_transcript(
        agent_id: AgentId,
        host_id: String,
        rows: Vec<crate::state::ChatRowHandle>,
    ) -> HtmlElement {
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.chat_rows.update(|map| {
                map.insert(agent_id_mount.clone(), rows.clone());
            });
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_001) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        container
    }

    fn query(container: &HtmlElement, selector: &str) -> Option<Element> {
        container
            .query_selector(selector)
            .unwrap()
            .and_then(|node| node.dyn_into::<Element>().ok())
    }

    fn tycode_terminal_message() -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 1,
            sender: MessageSender::Assistant {
                agent: "tycode".to_owned(),
            },
            content: "TYCODE_GENUINE_7F3A_DONE".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn tycode_terminal_row() -> crate::state::ChatRowHandle {
        crate::state::ChatRowHandle::new(ChatMessageEntry {
            message: tycode_terminal_message(),
            tool_requests: Vec::new(),
        })
    }

    fn tycode_rendered_task_replay() -> [TaskList; 3] {
        let list = |first, second| TaskList {
            title: "TYCODE GENUINE 7F3A".to_owned(),
            tasks: vec![
                Task {
                    id: 0,
                    description: "7F3A establish genuine runtime task".to_owned(),
                    status: first,
                },
                Task {
                    id: 1,
                    description: "7F3A prove status transition".to_owned(),
                    status: second,
                },
            ],
        };
        [
            list(TaskStatus::InProgress, TaskStatus::Pending),
            list(TaskStatus::Completed, TaskStatus::InProgress),
            list(TaskStatus::Completed, TaskStatus::Completed),
        ]
    }

    fn tycode_completed_task_list() -> TaskList {
        tycode_rendered_task_replay()[2].clone()
    }

    fn tycode_constructor_only_bootstrap() -> AgentBootstrapPayload {
        AgentBootstrapPayload {
            events: vec![
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(tycode_terminal_message())),
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(false)),
            ],
            latest_output: Default::default(),
            turn_active: false,
        }
    }

    fn tycode_genuine_task_bootstrap() -> AgentBootstrapPayload {
        let mut events = tycode_rendered_task_replay()
            .into_iter()
            .map(ChatEvent::TaskUpdate)
            .map(AgentBootstrapEvent::ChatEvent)
            .collect::<Vec<_>>();
        events.push(AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(
            tycode_terminal_message(),
        )));
        events.push(AgentBootstrapEvent::ChatEvent(
            ChatEvent::TypingStatusChanged(false),
        ));
        AgentBootstrapPayload {
            events,
            latest_output: Default::default(),
            turn_active: false,
        }
    }

    fn assert_constructor_only_terminal_render(container: &HtmlElement, phase: &str) {
        let panel = query(container, ".task-list-panel").expect("task panel shell");
        assert!(
            panel
                .get_attribute("class")
                .is_some_and(|classes| classes.split_whitespace().any(|class| class == "hidden")),
            "{phase}: constructor-only replay must keep the task panel hidden"
        );
        assert!(
            query(container, "[data-summary-action='tasks']").is_none(),
            "{phase}: constructor-only replay must not offer an unavailable task view"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("TYCODE_GENUINE_7F3A_DONE"), "{phase}: {text}");
        assert!(!text.contains("Initialization"), "{phase}: {text}");
        assert!(!text.contains("Deployment"), "{phase}: {text}");
        assert!(query(container, ".chat-streaming").is_none(), "{phase}");
    }

    fn assert_genuine_terminal_task_render(container: &HtmlElement, phase: &str) {
        assert!(
            !query(container, ".summary-task-view")
                .expect("task view")
                .has_attribute("hidden"),
            "{phase}: the selected task view must survive reactive updates"
        );
        assert_eq!(
            query(container, ".task-list-heading")
                .expect("task heading")
                .text_content()
                .unwrap_or_default(),
            "TYCODE GENUINE 7F3A",
            "{phase}"
        );
        assert_eq!(
            query(container, ".task-list-progress")
                .expect("task progress")
                .text_content()
                .unwrap_or_default(),
            "2/2 tasks completed",
            "{phase}"
        );
        let rows = container
            .query_selector_all(".task-item-row.status-completed")
            .expect("completed rows");
        assert_eq!(rows.length(), 2, "{phase}");
        let descriptions = container
            .query_selector_all(".task-item-desc")
            .expect("task descriptions");
        assert_eq!(descriptions.length(), 2, "{phase}");
        assert_eq!(
            descriptions
                .item(0)
                .unwrap()
                .text_content()
                .unwrap_or_default(),
            "7F3A establish genuine runtime task",
            "{phase}"
        );
        assert_eq!(
            descriptions
                .item(1)
                .unwrap()
                .text_content()
                .unwrap_or_default(),
            "7F3A prove status transition",
            "{phase}"
        );
        let icons = container
            .query_selector_all(".task-item-icon")
            .expect("task icons");
        assert_eq!(icons.length(), 2, "{phase}");
        for index in 0..icons.length() {
            assert_eq!(
                icons
                    .item(index)
                    .unwrap()
                    .text_content()
                    .unwrap_or_default(),
                "✓",
                "{phase}: check mark {index}"
            );
        }
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("TYCODE_GENUINE_7F3A_DONE"), "{phase}: {text}");
        assert!(!text.contains("Initialization"), "{phase}: {text}");
        assert!(!text.contains("Deployment"), "{phase}: {text}");
        assert!(query(container, ".chat-streaming").is_none(), "{phase}");
    }

    #[wasm_bindgen_test]
    async fn constructor_only_terminal_replay_has_no_task_panel() {
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let state_for_mount = state_handle.clone();
        let agent_id = AgentId("tycode-constructor-only".to_owned());
        let agent_id_for_mount = agent_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut agent = make_target_agent("tycode-render-host", &agent_id_for_mount.0, None);
            agent.backend_kind = BackendKind::Tycode;
            state.agents.set(vec![agent]);
            state.chat_rows.update(|rows| {
                rows.insert(agent_id_for_mount.clone(), vec![tycode_terminal_row()]);
            });
            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let bound = ActiveAgentRef {
                host_id: "tycode-render-host".to_owned(),
                agent_id: agent_id_for_mount.clone(),
            };
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active = Signal::derive(|| false);
            view! { <ChatView tab_id=TabId(27_011) agent_ref=agent_ref is_active=is_active /> }
        });
        next_tick().await;
        assert_constructor_only_terminal_render(&container, "live");

        let state = state_handle
            .borrow()
            .as_ref()
            .expect("mounted state")
            .clone();
        for phase in ["reload", "resume"] {
            crate::dispatch::apply_agent_bootstrap_for_test(
                &state,
                "tycode-render-host",
                &StreamPath(format!("/agent/{}", agent_id.0)),
                tycode_constructor_only_bootstrap(),
            );
            next_tick().await;
            assert_constructor_only_terminal_render(&container, phase);
        }
        drop(handle);
        container.remove();
    }

    /// A reported-then-missing context keeps its panel, and says so.
    ///
    /// The regression: only the terminal message of a turn carries a
    /// breakdown, so a later assistant message without one made the derivation
    /// report "this agent has no context". That unmounted the whole summary
    /// panel — and with it the only control that switches back to the context
    /// view — leaving a reader stranded on the task list. A gap is `Unknown`,
    /// not absence. Contrast `constructor_only_terminal_replay_has_no_task_panel`,
    /// where occupancy was never reported and the panel must stay hidden.
    #[wasm_bindgen_test]
    async fn a_context_gap_keeps_the_panel_and_names_itself() {
        ensure_styles_loaded();
        let container = make_container();
        let agent_id = AgentId("claude-context-gap".to_owned());
        let agent_id_for_mount = agent_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut agent = make_target_agent("tycode-render-host", &agent_id_for_mount.0, None);
            agent.backend_kind = BackendKind::Claude;
            state.agents.set(vec![agent]);
            // A turn that reported occupancy, then a later message that did
            // not — exactly what a multi-phase Claude turn emits.
            let mut reported = tycode_terminal_message();
            reported.context_breakdown = Some(protocol::ContextBreakdown {
                system_prompt_bytes: 400,
                tool_io_bytes: 0,
                conversation_history_bytes: 0,
                reasoning_bytes: 0,
                context_injection_bytes: 0,
                input_tokens: 4_000,
                context_window: 200_000,
            });
            state.chat_rows.update(|rows| {
                rows.insert(
                    agent_id_for_mount.clone(),
                    vec![
                        crate::state::ChatRowHandle::new(ChatMessageEntry {
                            message: reported,
                            tool_requests: Vec::new(),
                        }),
                        tycode_terminal_row(),
                    ],
                );
            });
            provide_context(state);
            let bound = ActiveAgentRef {
                host_id: "tycode-render-host".to_owned(),
                agent_id: agent_id_for_mount.clone(),
            };
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active = Signal::derive(|| false);
            view! { <ChatView tab_id=TabId(27_013) agent_ref=agent_ref is_active=is_active /> }
        });
        next_tick().await;

        let panel = query(&container, ".task-list-panel").expect("task panel shell");
        assert!(
            !panel
                .get_attribute("class")
                .is_some_and(|classes| classes.split_whitespace().any(|class| class == "hidden")),
            "a gap in reported occupancy must not unmount the summary panel"
        );
        let context_text = query(&container, ".summary-context-view")
            .expect("context panel")
            .text_content()
            .unwrap_or_default();
        assert!(
            context_text.contains("Unavailable"),
            "a gap must name itself rather than draw an empty bar; got {context_text:?}"
        );

        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn genuine_task_panel_survives_reload_and_resume() {
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let state_for_mount = state_handle.clone();
        let agent_id = AgentId("tycode-genuine-task".to_owned());
        let agent_id_for_mount = agent_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut agent = make_target_agent("tycode-render-host", &agent_id_for_mount.0, None);
            agent.backend_kind = BackendKind::Tycode;
            state.agents.set(vec![agent]);
            state.chat_rows.update(|rows| {
                rows.insert(agent_id_for_mount.clone(), vec![tycode_terminal_row()]);
            });
            state.task_lists.update(|lists| {
                lists.insert(agent_id_for_mount.clone(), tycode_completed_task_list());
            });
            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let bound = ActiveAgentRef {
                host_id: "tycode-render-host".to_owned(),
                agent_id: agent_id_for_mount.clone(),
            };
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active = Signal::derive(|| false);
            view! { <ChatView tab_id=TabId(27_012) agent_ref=agent_ref is_active=is_active /> }
        });
        next_tick().await;
        query(&container, "[data-summary-action='tasks']")
            .expect("genuine task link")
            .dyn_into::<HtmlElement>()
            .expect("task link button")
            .click();
        next_tick().await;
        assert_genuine_terminal_task_render(&container, "live");

        let state = state_handle
            .borrow()
            .as_ref()
            .expect("mounted state")
            .clone();
        for phase in ["reload", "resume"] {
            state.task_lists.update(|lists| {
                lists.remove(&agent_id);
            });
            next_tick().await;
            crate::dispatch::apply_agent_bootstrap_for_test(
                &state,
                "tycode-render-host",
                &StreamPath(format!("/agent/{}", agent_id.0)),
                tycode_genuine_task_bootstrap(),
            );
            next_tick().await;
            assert_genuine_terminal_task_render(&container, phase);
        }
        drop(handle);
        container.remove();
    }

    /// A marker is a timeline divider, not a chat card. It carries no sender
    /// label, no copy control, and no message body — an artifact that acquires
    /// a sender is exactly the "raw summary rendered as a user message" leak
    /// this work exists to close.
    #[wasm_bindgen_test]
    async fn compaction_marker_renders_as_divider_not_chat_card() {
        ensure_styles_loaded();
        let container = mount_transcript(
            AgentId("agent-marker".to_owned()),
            "host-marker".to_owned(),
            vec![
                crate::state::ChatRowHandle::new(mk_user_msg("before compaction")),
                crate::state::ChatRowHandle::context_compaction(compaction_marker_event(
                    "m1",
                    ContextCompactionTimelineStatus::Completed,
                    CompactionMutation::Completed,
                    full_metrics(),
                )),
                crate::state::ChatRowHandle::new(mk_user_msg("after compaction")),
            ],
        );
        next_tick().await;

        let rows = message_rows(&container);
        assert_eq!(rows.len(), 3, "the marker occupies one transcript row");

        let marker = &rows[1];
        let text = marker.text_content().unwrap_or_default();
        assert!(
            text.contains("Context compacted"),
            "the marker states what happened: {text}"
        );
        // Corrected assertion. Evidence: the rendered marker text is
        // "Context compacted. 384,168 tokens reduced to 12,518. Took 2 minutes
        // 50 seconds. Your conversation history here is unchanged." — the
        // accessible sentence — so a substring test for "You" matches the word
        // "Your" and rejects correct output. The contract it was reaching for
        // is "the marker carries no sender label", and `.chat-card-sender` is
        // the element that renders one. Asserting its absence is strictly
        // sharper: it covers every sender (User/System/Warning/Error/Assistant)
        // rather than two literals, and prose can never satisfy it.
        assert!(
            marker
                .query_selector(".chat-card-sender")
                .unwrap()
                .is_none(),
            "a marker carries no sender label element: {text}"
        );
        assert!(
            marker.query_selector(".chat-card").unwrap().is_none(),
            "a marker is not rendered as a chat card"
        );
        assert!(
            marker.query_selector("button").unwrap().is_none(),
            "a marker offers no copy or tool controls"
        );

        // Both neighbours survive: compaction changes model context, never the
        // visible transcript.
        let all = container.text_content().unwrap_or_default();
        assert!(
            all.contains("before compaction") && all.contains("after compaction"),
            "the full transcript stays visible either side of the marker: {all}"
        );
    }

    /// The figures are the point of the marker for anyone scanning back through
    /// a long session.
    #[wasm_bindgen_test]
    async fn completed_marker_shows_before_after_tokens_and_duration() {
        ensure_styles_loaded();
        let container = mount_transcript(
            AgentId("agent-metrics".to_owned()),
            "host-metrics".to_owned(),
            vec![crate::state::ChatRowHandle::context_compaction(
                compaction_marker_event(
                    "m2",
                    ContextCompactionTimelineStatus::Completed,
                    CompactionMutation::Completed,
                    full_metrics(),
                ),
            )],
        );
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("384.2K"), "before-token figure: {text}");
        assert!(text.contains("12.5K"), "after-token figure: {text}");
        assert!(text.contains("2m 50s"), "elapsed duration: {text}");
    }

    /// Metric coverage differs per backend and absence is normal. A missing
    /// figure renders as nothing — never a zero, which would read as
    /// "compacted to nothing", and never a placeholder dash.
    #[wasm_bindgen_test]
    async fn unknown_metrics_render_no_figures_at_all() {
        ensure_styles_loaded();
        let container = mount_transcript(
            AgentId("agent-nometrics".to_owned()),
            "host-nometrics".to_owned(),
            vec![crate::state::ChatRowHandle::context_compaction(
                compaction_marker_event(
                    "m3",
                    ContextCompactionTimelineStatus::Completed,
                    CompactionMutation::Completed,
                    protocol::CompactionMetrics::default(),
                ),
            )],
        );
        next_tick().await;

        let marker = query(&container, ".context-compaction-marker").expect("marker row");
        let text = marker.text_content().unwrap_or_default();
        assert!(
            text.contains("Context compacted"),
            "title still renders: {text}"
        );
        assert!(!text.contains('0'), "no fabricated zero figure: {text}");
        assert!(!text.contains('→'), "no empty before/after arrow: {text}");
        assert_eq!(
            marker
                .query_selector_all(".context-compaction-metric")
                .unwrap()
                .length(),
            0,
            "absent metrics produce no metric elements at all"
        );
    }

    /// What the user needs first from a failure is whether their model context
    /// survived — not the provider's prose.
    #[wasm_bindgen_test]
    async fn failed_marker_states_whether_context_changed() {
        ensure_styles_loaded();
        let container = mount_transcript(
            AgentId("agent-failed".to_owned()),
            "host-failed".to_owned(),
            vec![
                crate::state::ChatRowHandle::context_compaction(compaction_marker_event(
                    "m4",
                    ContextCompactionTimelineStatus::Failed,
                    CompactionMutation::NotObserved,
                    protocol::CompactionMetrics::default(),
                )),
                crate::state::ChatRowHandle::context_compaction(compaction_marker_event(
                    "m5",
                    ContextCompactionTimelineStatus::Failed,
                    CompactionMutation::MayHaveMutated,
                    protocol::CompactionMetrics::default(),
                )),
            ],
        );
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("context unchanged"),
            "a pre-mutation failure says the context is intact: {text}"
        );
        assert!(
            text.contains("may have changed"),
            "a post-dispatch failure says the context is uncertain: {text}"
        );
    }

    /// Historical rows must never be live regions. The virtualizer mounts and
    /// unmounts them as the user scrolls, and bootstrap/paging re-mount them
    /// wholesale — a `status` or `alert` role here announces a compaction from
    /// days ago, repeatedly.
    #[wasm_bindgen_test]
    async fn marker_row_is_not_a_live_region_and_exposes_one_full_sentence() {
        ensure_styles_loaded();
        let container = mount_transcript(
            AgentId("agent-a11y".to_owned()),
            "host-a11y".to_owned(),
            vec![crate::state::ChatRowHandle::context_compaction(
                compaction_marker_event(
                    "m6",
                    ContextCompactionTimelineStatus::Completed,
                    CompactionMutation::Completed,
                    full_metrics(),
                ),
            )],
        );
        next_tick().await;

        let marker = query(&container, ".context-compaction-marker").expect("marker row");
        assert!(
            marker.get_attribute("aria-live").is_none(),
            "a durable row must not be a live region"
        );
        // V1 is a plain element: no `status`/`alert` (the virtualizer remounts
        // this row on every scroll pass), and no `separator`/`group` either
        // until real assistive-technology evidence justifies one.
        assert!(
            marker.get_attribute("role").is_none(),
            "the durable marker is a plain element in V1"
        );

        // Exactly one representation in the accessibility tree: the whole
        // visible row is hidden, and the visually-hidden sentence carries the
        // figures in full. Grouped digits and a spoken duration, because
        // "384168" is read digit-by-digit and "2m 50s" is read as letters.
        let visual = query(&container, ".context-compaction-visual").expect("visible row");
        assert_eq!(
            visual.get_attribute("aria-hidden").as_deref(),
            Some("true"),
            "the abbreviated visible metrics are hidden from assistive technology"
        );
        let sentence = query(&container, ".context-compaction-marker .visually-hidden")
            .expect("accessible sentence")
            .text_content()
            .unwrap_or_default();
        assert!(
            sentence.contains("384,168") && sentence.contains("12,518"),
            "grouped exact counts: {sentence}"
        );
        assert!(
            sentence.contains("2 minutes 50 seconds"),
            "and the humanized duration, which the visible row abbreviates: {sentence}"
        );
        assert!(
            sentence.contains("unchanged"),
            "and the guarantee the user is most likely to doubt: {sentence}"
        );

        // The visible abbreviations must not *also* be exposed.
        assert!(
            !marker.has_attribute("aria-label"),
            "naming the row would leave the hidden sentence and the visible \
             fragments both in the tree"
        );
    }

    /// The live banner is the only compaction surface that announces, and it
    /// is atomic: a stage change on its own ("Finalizing.") is meaningless
    /// read in isolation.
    #[wasm_bindgen_test]
    async fn live_banner_is_polite_atomic_status_and_promises_the_transcript() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-banner".to_owned());
        let host_id = "host-banner".to_owned();
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.context_compactions.update(|map| {
                map.insert(
                    agent_id_mount.clone(),
                    ContextCompactionUiState::Active {
                        live: true,
                        payload: Box::new(protocol::ContextCompactionNotifyPayload {
                            operation_id: protocol::CompactionOperationId("op-1".to_owned()),
                            agent_id: agent_id_mount.clone(),
                            logical_session_id: protocol::SessionId("s".to_owned()),
                            backend_kind: BackendKind::Claude,
                            trigger: CompactionTrigger::UserRequested,
                            method: Some(CompactionMethod::NativeTextCommand),
                            status: ContextCompactionStatus::Progress {
                                stage: CompactionStage::Compacting,
                            },
                            provider_version: None,
                            metrics: protocol::CompactionMetrics::default(),
                            message: None,
                        }),
                    },
                );
            });
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_002) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let banner = query(&container, "[data-test='context-compaction-banner']")
            .expect("live banner while an operation runs");
        assert_eq!(banner.get_attribute("role").as_deref(), Some("status"));
        assert_eq!(banner.get_attribute("aria-live").as_deref(), Some("polite"));
        assert_eq!(banner.get_attribute("aria-atomic").as_deref(), Some("true"));

        let text = banner.text_content().unwrap_or_default();
        assert!(
            text.contains("Compacting context"),
            "the banner says what is happening: {text}"
        );
        assert!(
            text.contains("history here is unchanged"),
            "and restates the guarantee during the multi-minute wait: {text}"
        );

        // The elapsed counter is inside an atomic live region, so it must be
        // hidden or every tick re-announces the whole banner.
        let elapsed = query(&container, ".compacting-card-elapsed")
            .expect("a multi-minute operation shows elapsed time");
        assert_eq!(
            elapsed.get_attribute("aria-hidden").as_deref(),
            Some("true"),
            "a per-second counter must not drive an atomic live region"
        );
    }

    /// A banner reconstructed from `AgentBootstrap` must be *visible* but
    /// silent: inserting a node into an `aria-live` region is itself an
    /// announcement, so suppressing the explicit announce call is not enough.
    #[wasm_bindgen_test]
    async fn bootstrap_restored_banner_is_visible_but_not_a_live_region() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-restored".to_owned());
        let host_id = "host-restored".to_owned();
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.context_compactions.update(|map| {
                map.insert(
                    agent_id_mount.clone(),
                    ContextCompactionUiState::Active {
                        // As restored by bootstrap.
                        live: false,
                        payload: Box::new(protocol::ContextCompactionNotifyPayload {
                            operation_id: protocol::CompactionOperationId("op-boot".to_owned()),
                            agent_id: agent_id_mount.clone(),
                            logical_session_id: protocol::SessionId("s".to_owned()),
                            backend_kind: BackendKind::Claude,
                            trigger: CompactionTrigger::UserRequested,
                            method: Some(CompactionMethod::NativeTextCommand),
                            status: ContextCompactionStatus::Progress {
                                stage: CompactionStage::Compacting,
                            },
                            provider_version: None,
                            metrics: protocol::CompactionMetrics::default(),
                            message: None,
                        }),
                    },
                );
            });
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_005) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let banner = query(&container, "[data-test='context-compaction-banner']")
            .expect("a reconnect still shows the running operation");
        assert!(
            banner
                .text_content()
                .unwrap_or_default()
                .contains("Compacting context"),
            "the operation is visible after a reconnect"
        );
        assert_eq!(
            banner.get_attribute("aria-live").as_deref(),
            Some("off"),
            "but mounting it must not announce old work as if it were new"
        );
    }

    /// A terminal failure is explained by the durable marker row, never by a
    /// retained banner — and the row is never an alert. The one assertive
    /// announcement happens at the live transition; alert semantics here would
    /// replay it on every remount and route change.
    ///
    /// Corrected assertion. Evidence: the previous version constructed
    /// `ContextCompactionUiState::Failed` by hand and asserted the *banner*
    /// carried the explanation. The banner is mounted outside the windowed list
    /// (see `ContextCompactionBanner`), so a card retained there has no row and
    /// therefore no position — it stayed pinned to the end of the transcript
    /// while later turns rendered above it, which is the reported bug. The
    /// contract being reached for was "a failure stays explained on screen,
    /// says what failed and what it means for the user's context, and never
    /// re-announces". All three are preserved and re-pointed at the surface
    /// that actually owns the outcome, plus the anchoring guarantee the old
    /// assertion could not express.
    #[wasm_bindgen_test]
    async fn terminal_failure_is_explained_by_the_marker_row_not_a_retained_banner() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-failbanner".to_owned());
        let host_id = "host-failbanner".to_owned();
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            // The server's frozen ordering for a terminal compaction: durable
            // marker, then terminal notify. Drive both through the real
            // reducers rather than hand-building UI state.
            let mut marker = compaction_marker_event(
                "operation:op-failed",
                ContextCompactionTimelineStatus::Failed,
                CompactionMutation::MayHaveMutated,
                protocol::CompactionMetrics::default(),
            );
            marker.message = Some("summarizer timed out".to_owned());
            state.push_compaction_marker(agent_id_mount.clone(), marker);
            state.apply_context_compaction_notify(
                &agent_id_mount,
                protocol::ContextCompactionNotifyPayload {
                    operation_id: protocol::CompactionOperationId("op-failed".to_owned()),
                    agent_id: agent_id_mount.clone(),
                    logical_session_id: protocol::SessionId("s".to_owned()),
                    backend_kind: BackendKind::Claude,
                    trigger: CompactionTrigger::UserRequested,
                    method: Some(CompactionMethod::NativeTextCommand),
                    status: ContextCompactionStatus::Failed {
                        accepted: true,
                        mutation: CompactionMutation::MayHaveMutated,
                    },
                    provider_version: None,
                    metrics: protocol::CompactionMetrics::default(),
                    message: Some("summarizer timed out".to_owned()),
                },
                true,
            );
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_006) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let marker = query(&container, "[data-test='context-compaction-marker']")
            .expect("the failure stays explained on screen");
        let text = marker.text_content().unwrap_or_default();
        assert!(
            text.contains("summarizer timed out") && text.contains("may have changed"),
            "saying what failed and what it means: {text}"
        );
        assert!(
            marker.get_attribute("role").is_none(),
            "a historical row carries no live-region role, so a remount cannot replay it"
        );
        assert!(
            marker.get_attribute("aria-live").is_none(),
            "and is not a live region at all"
        );
        assert!(
            query(&container, "[data-test='context-compaction-banner']").is_none(),
            "and no banner is left behind: a banner has no row, so it would sit \
             at the tip of the transcript for the rest of the session"
        );
    }

    /// The reported bug, as geometry. A compaction that failed at one point in
    /// the conversation must stay at that point: turns that arrive afterwards
    /// render *below* it. Before the fix the failure lived in a banner mounted
    /// after the windowed list, so every later turn appeared above it and the
    /// card drifted to the end of the transcript.
    #[wasm_bindgen_test]
    async fn a_failed_compaction_stays_where_it_happened_when_later_turns_arrive() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-anchor".to_owned());
        let host_id = "host-anchor".to_owned();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();

        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.chat_rows.update(|map| {
                map.insert(
                    agent_id_mount.clone(),
                    vec![ChatRowHandle::new(mk_user_msg("continue"))],
                );
            });
            let mut marker = compaction_marker_event(
                "operation:op-anchor",
                ContextCompactionTimelineStatus::Failed,
                CompactionMutation::NotObserved,
                protocol::CompactionMetrics::default(),
            );
            marker.message = Some("OAuth session expired".to_owned());
            state.push_compaction_marker(agent_id_mount.clone(), marker);
            state.apply_context_compaction_notify(
                &agent_id_mount,
                protocol::ContextCompactionNotifyPayload {
                    operation_id: protocol::CompactionOperationId("op-anchor".to_owned()),
                    agent_id: agent_id_mount.clone(),
                    logical_session_id: protocol::SessionId("s".to_owned()),
                    backend_kind: BackendKind::Claude,
                    trigger: CompactionTrigger::UserRequested,
                    method: Some(CompactionMethod::NativeTextCommand),
                    status: ContextCompactionStatus::Failed {
                        accepted: false,
                        mutation: CompactionMutation::NotObserved,
                    },
                    provider_version: None,
                    metrics: protocol::CompactionMetrics::default(),
                    message: Some("OAuth session expired".to_owned()),
                },
                true,
            );
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_007) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        // Two turns land after the failure, exactly as in the report.
        let state = state_handle.borrow().clone().expect("state was captured");
        state.chat_rows.update(|map| {
            let rows = map.get_mut(&agent_id).expect("transcript exists");
            rows.push(ChatRowHandle::new(mk_user_msg("later turn one")));
            rows.push(ChatRowHandle::new(mk_user_msg("later turn two")));
        });
        next_tick().await;

        // One failure, one surface. The defect was a *second* rendering of the
        // same failure, so the count of surfaces is the assertion — not the
        // count of words: the marker deliberately carries its reason twice,
        // once visibly and once in a `visually-hidden` sentence, and
        // `text_content` does not honour `aria-hidden`.
        let surfaces = container
            .query_selector_all(
                "[data-test='context-compaction-marker'], \
                 [data-test='context-compaction-banner']",
            )
            .unwrap()
            .length();
        assert_eq!(
            surfaces, 1,
            "a failed compaction is reported exactly once; a second copy has no \
             transcript row and so cannot stay at the turn it describes"
        );

        let marker = query(&container, "[data-test='context-compaction-marker']")
            .expect("the failure is still on screen after later turns arrive");
        let marker_bottom = marker.get_bounding_client_rect().bottom();
        let rows = message_rows(&container);
        let later = rows
            .iter()
            .filter(|row| {
                let text = row.text_content().unwrap_or_default();
                text.contains("later turn one") || text.contains("later turn two")
            })
            .collect::<Vec<_>>();
        assert_eq!(later.len(), 2, "both later turns rendered");
        for row in later {
            let text = row.text_content().unwrap_or_default();
            assert!(
                row.get_bounding_client_rect().top() >= marker_bottom,
                "a turn that happened after the compaction failure must render \
                 below it, not above; {text:?} rendered at {} with the failure \
                 ending at {marker_bottom}",
                row.get_bounding_client_rect().top(),
            );
        }
    }

    /// A rate-limit retry is about one specific turn, so it belongs at that
    /// turn. Two failures before the fix, both visible here: the notice lived
    /// in a card rendered after the windowed list, so later turns appeared
    /// above it and it drifted to the end of the transcript; and it was cleared
    /// wholesale at the next `StreamStart`, so recovering from the rate limit
    /// erased the record that it had ever happened.
    #[wasm_bindgen_test]
    async fn a_retry_notice_stays_at_its_turn_when_the_stream_recovers() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-retry".to_owned());
        let host_id = "host-retry".to_owned();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();

        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.chat_rows.update(|map| {
                map.insert(
                    agent_id_mount.clone(),
                    vec![ChatRowHandle::new(mk_user_msg("continue"))],
                );
            });
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_008) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let state = state_handle.borrow().clone().expect("state was captured");
        crate::dispatch::apply_chat_event(
            &state,
            &host_id,
            &agent_id,
            ChatEvent::RetryAttempt(protocol::RetryAttemptData {
                attempt: 1,
                max_retries: 3,
                error: "429 Too Many Requests".to_owned(),
                backoff_ms: 4_000,
            }),
        );
        next_tick().await;

        // The provider recovers and the turn continues.
        crate::dispatch::apply_chat_event(
            &state,
            &host_id,
            &agent_id,
            ChatEvent::StreamStart(protocol::StreamStartData {
                agent: "claude".to_owned(),
                model: Some("claude-opus-5".to_owned()),
            }),
        );
        state.chat_rows.update(|map| {
            let rows = map.get_mut(&agent_id).expect("transcript exists");
            rows.push(ChatRowHandle::new(mk_user_msg("later turn one")));
            rows.push(ChatRowHandle::new(mk_user_msg("later turn two")));
        });
        next_tick().await;

        // Selector-independent: recovering from the rate limit used to clear the
        // notice outright, so the provider's own error text is what proves the
        // record survived.
        let transcript = container.text_content().unwrap_or_default();
        assert!(
            transcript.contains("429 Too Many Requests"),
            "recovering from a rate limit must not erase the record of it: {transcript}"
        );

        let notice = query(&container, "[data-test='chat-notice-retry']")
            .expect("the surviving record renders as a retry notice");
        let text = notice.text_content().unwrap_or_default();
        assert!(
            text.contains("429 Too Many Requests") && text.contains("Attempt 1 of 3"),
            "the notice still says what happened and which attempt: {text}"
        );

        let notice_bottom = notice.get_bounding_client_rect().bottom();
        let later = message_rows(&container)
            .into_iter()
            .filter(|row| {
                let text = row.text_content().unwrap_or_default();
                text.contains("later turn one") || text.contains("later turn two")
            })
            .collect::<Vec<_>>();
        assert_eq!(later.len(), 2, "both later turns rendered");
        for row in later {
            let top = row.get_bounding_client_rect().top();
            assert!(
                top >= notice_bottom,
                "a turn that happened after the retry must render below it, not \
                 above; {:?} rendered at {top} with the notice ending at {notice_bottom}",
                row.text_content().unwrap_or_default(),
            );
        }
    }

    /// A deferred operation is waiting for a safe point, not hung. Saying so
    /// is the difference between "queued" and "broken".
    #[wasm_bindgen_test]
    async fn deferred_operation_reads_as_queued_not_stalled() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-deferred".to_owned());
        let host_id = "host-deferred".to_owned();
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            state.context_compactions.update(|map| {
                map.insert(
                    agent_id_mount.clone(),
                    ContextCompactionUiState::Active {
                        live: true,
                        payload: Box::new(protocol::ContextCompactionNotifyPayload {
                            operation_id: protocol::CompactionOperationId("op-2".to_owned()),
                            agent_id: agent_id_mount.clone(),
                            logical_session_id: protocol::SessionId("s".to_owned()),
                            backend_kind: BackendKind::Claude,
                            trigger: CompactionTrigger::UserRequested,
                            method: None,
                            status: ContextCompactionStatus::Deferred {
                                stage: CompactionStage::WaitingForIdle,
                            },
                            provider_version: None,
                            metrics: protocol::CompactionMetrics::default(),
                            message: None,
                        }),
                    },
                );
            });
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_003) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let banner = query(&container, "[data-test='context-compaction-banner']")
            .expect("banner for a deferred operation");
        let text = banner.text_content().unwrap_or_default();
        assert!(
            text.contains("Compaction queued"),
            "queued, not compacting: {text}"
        );
        assert!(
            text.contains("Waiting for the current turn to finish"),
            "and says what it is waiting for: {text}"
        );
    }

    /// The header control stays visible and explains itself when it cannot be
    /// used. Hiding it — the previous behaviour — teaches the user it does not
    /// exist, and the most common blocker is transient.
    #[wasm_bindgen_test]
    async fn header_compact_control_is_visible_and_explains_why_it_is_disabled() {
        ensure_styles_loaded();
        let agent_id = AgentId("agent-header".to_owned());
        let host_id = "host-header".to_owned();
        let container = make_container();
        let agent_id_mount = agent_id.clone();
        let host_id_mount = host_id.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_mount.clone(),
                agent_id: agent_id_mount.clone(),
            };
            // Disconnected host: a real, explainable blocker.
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_004) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let button = query(&container, "[data-test='chat-header-compact']")
            .expect("the control stays in the header even when unavailable");
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "and is disabled rather than hidden"
        );
        let label = button.get_attribute("aria-label").unwrap_or_default();
        assert!(
            label.contains("unavailable:"),
            "the accessible name carries the reason: {label}"
        );

        // A natively-disabled button leaves the tab order, which makes the
        // reason hover-only. `aria-disabled` keeps it focusable so a keyboard
        // or screen-reader user can reach the explanation.
        assert!(
            !button.has_attribute("disabled"),
            "the reason must be reachable without a pointer"
        );
    }
}
