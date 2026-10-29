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
use crate::components::ink_transcript::{InkGap, InkRowLayout, ink_row_layouts};
use crate::components::orchestration_view::OrchestrationView;
use crate::components::settings_panel::persist_tool_output_mode;
use crate::components::task_list::TaskListView;
use crate::perf::{CaptureSpan, ChatProjection};
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
/// in lockstep with the CSS rule. Ink spaces rows with padding inside
/// the measured row instead, so its gap is zero.
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
        "Choose Orchestrator from the New Chat \u{25be} chooser: every backend drafts a plan, the plans cross-review to consensus, one agent implements, and the other backends review the result.",
    ),
    (
        "Ask Help",
        "Choose Help from the New Chat \u{25be} chooser to ask how anything in Tyde works \u{2014} it can change settings and run other agents for you.",
    ),
    (
        "Customize your default agent",
        "Edit the Default agent in Settings \u{2192} Custom Agents to shape ordinary project chats. Fresh Home chats start with Help.",
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
        let mut span = CaptureSpan::new(ChatProjection::Rows);
        let Some(id) = active_agent_id() else {
            return Vec::new();
        };
        let mut rows = state
            .chat_rows
            .with(|m| m.get(&id).cloned().unwrap_or_default());
        if let Some(stream) = state.streaming_text.with(|m| m.get(&id).cloned()) {
            let index = stream.insertion_index(&rows);
            rows.insert(
                index,
                ChatRowHandle {
                    id: stream.row_id,
                    content: ChatRowContent::Streaming(stream),
                },
            );
        }
        if state.chat_appearance.get() != crate::appearance::ChatAppearance::Ink {
            rows.retain(|row| !matches!(&row.content, ChatRowContent::Notice(notice) if notice.with(|notice| matches!(notice, ChatNotice::RetrySequence { .. }))));
        }
        span.items(rows.len());
        rows
    };

    // Ink groups the transcript into stretches; Current renders rows alone
    // and subscribes to nothing here.
    let ink_state = state.clone();
    let ink_layouts: Memo<Option<Arc<HashMap<ChatRowId, InkRowLayout>>>> = Memo::new(move |_| {
        let appearance = ink_state.chat_appearance.get();
        if !appearance.uses_ink_chrome() {
            return None;
        }
        let flow_show_reasoning = ink_state.flow_show_reasoning.get();
        active_agent_id()?;
        let mut span = CaptureSpan::new(ChatProjection::InkLayout);
        let rows = row_handles();
        span.items(rows.len());
        Some(Arc::new(ink_row_layouts(
            &rows,
            appearance,
            flow_show_reasoning,
        )))
    });

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

    let native_goals = state.native_goals;
    let native_goal_capabilities = state.goal_capabilities;
    let goal_action_state = state.clone();
    let on_goal_control = Callback::new(move |control: protocol::GoalControl| {
        if let Some(agent) = agent_ref.get_untracked() {
            let state = goal_action_state.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) =
                    crate::actions::control_native_goal(&state, agent.clone(), control).await
                {
                    crate::notices::report_agent_error(&agent, &error);
                }
            });
        }
    });
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

        let mut span = CaptureSpan::new(ChatProjection::ContextBreakdown);
        state.chat_rows.with(|rows_by_agent| {
            let rows = rows_by_agent.get(&active.agent_id)?;
            for row in rows.iter().rev() {
                span.visit();
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
        let mut span = CaptureSpan::new(ChatProjection::ContextReported);
        state.chat_rows.with(|rows_by_agent| {
            rows_by_agent.get(&active.agent_id).is_some_and(|rows| {
                rows.iter().any(|row| {
                    span.visit();
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
        let mut span = CaptureSpan::new(ChatProjection::VirtualWindow);
        let _ = heights_version.get();
        let st = scroll_top_sig.get();
        let vp = viewport_height_sig.get();
        let layouts = ink_layouts.get();
        let row_gap = if layouts.is_some() { 0.0 } else { ROW_GAP_PX };
        let rows = row_handles();
        let n = rows.len();
        span.items(n);
        if n == 0 {
            return VirtualWindow::EMPTY;
        }
        row_heights.with_value(|map| {
            // `slot_height` includes the top margin that separates this
            // row from the previous one, so the running sum matches
            // what the browser actually lays out. The very first row
            // gets no leading gap.
            let slot_height = |idx: usize| -> f64 {
                if layouts
                    .as_ref()
                    .and_then(|layouts| layouts.get(&rows[idx].id))
                    .is_some_and(|layout| layout.empty)
                {
                    return 0.0;
                }
                let raw = map
                    .get(&rows[idx].id)
                    .copied()
                    .unwrap_or(ESTIMATED_ROW_HEIGHT);
                if idx == 0 { raw } else { raw + row_gap }
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
        <crate::components::chat_presentation::ChatPresentation>
          <crate::notices::InlineNotices scopes=Signal::derive(move || agent_ref.get()
              .map(|agent| vec![crate::notices::NoticeScope::Agent(agent.host_id, agent.agent_id)])
              .unwrap_or_default()) />
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
                            BackendKind::Muse => ("backend-badge muse", "Muse"),
                        };
                        view! { <span class=badge_class>{label}</span> }
                    })}
                    <Show when=move || owns_composer.get()>
                        <ToolOutputModeToggle />
                    </Show>
                    <ReviewChangesButton agent_ref=agent_ref />
                    <AgentTydePermissionsMenu agent_ref=agent_ref />
                </div>
                <TaskListView
                    agent_id=Signal::derive(move || {
                        agent_ref.get().map(|active| active.agent_id)
                    })
                    goal=Signal::derive(move || agent_ref.get().and_then(|agent| native_goals.with(|map| map.get(&agent.agent_id).cloned())))
                    goal_capabilities=Signal::derive(move || agent_ref.get().and_then(|agent| native_goal_capabilities.with(|map| map.get(&agent.agent_id).cloned())))
                    on_goal_control=on_goal_control
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
                                ink_layouts=ink_layouts
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
            <Show when=move || !has_composer.get()>
                <InflightTray agent_ref=agent_ref />
            </Show>
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
        </crate::components::chat_presentation::ChatPresentation>
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
    ink_layouts: Memo<Option<Arc<HashMap<ChatRowId, InkRowLayout>>>>,
) -> impl IntoView {
    let row_id = row.id;
    let ink_layout: Memo<Option<InkRowLayout>> = Memo::new(move |_| {
        ink_layouts.with(|layouts| {
            layouts
                .as_ref()
                .map(|layouts| layouts.get(&row_id).cloned().unwrap_or_default())
        })
    });
    let row_class = move || {
        ink_layout.with(|layout| {
            let Some(layout) = layout else {
                return "virt-row".to_owned();
            };
            let mut class = String::from("virt-row ink-row");
            match layout.gap {
                InkGap::None => class.push_str(" ink-gap-none"),
                InkGap::Tight => class.push_str(" ink-gap-tight"),
                InkGap::Normal => {}
            }
            if layout.continuation {
                class.push_str(" ink-continuation");
            }
            if layout.actions_only {
                class.push_str(" ink-actions-only");
            }
            if layout.empty {
                class.push_str(" ink-empty");
            }
            class
        })
    };
    let ink_layout_signal: Signal<Option<InkRowLayout>> = ink_layout.into();
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
        <div class=row_class node_ref=node_ref>
            {match row.content {
                ChatRowContent::Streaming(streaming) => {
                    view! { <ChatStreamingView agent_ref=agent_ref streaming=streaming ink_layout=ink_layout_signal /> }.into_any()
                }
                ChatRowContent::Message(entry) => {
                    view! { <ChatMessageView agent_ref=agent_ref entry=entry ink_layout=ink_layout_signal /> }.into_any()
                }
                ChatRowContent::ContextCompaction(event) => {
                    view! { <ContextCompactionMarker event=event /> }.into_any()
                }
                ChatRowContent::Notice(notice) => {
                    view! { <ChatNoticeView agent_ref=agent_ref notice=notice /> }.into_any()
                }
            }}
        </div>
    }
}

// ── Context compaction ──────────────────────────────────────────────────

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

#[component]
fn AgentHandoffView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    handoff: protocol::AgentHandoff,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let target_state = state.clone();
    let target = Memo::new(move |_| {
        let source = agent_ref.get()?;
        target_state.agents.with(|agents| {
            agents
                .iter()
                .find(|agent| agent.host_id == source.host_id && agent.agent_id == handoff.agent_id)
                .cloned()
        })
    });
    let label_state = state.clone();
    let project = move || {
        let source = agent_ref.get()?;
        let project_id = handoff.project_id.as_ref()?;
        label_state.projects.with(|projects| {
            projects
                .iter()
                .find(|project| {
                    project.host_id == source.host_id && &project.project.id == project_id
                })
                .map(|project| project.project.name.clone())
        })
    };
    view! {
        <div class="chat-card chat-card-system">
            <div class="chat-card-body">
                <p>"Independent agent: "{move || target.get().map(|agent| agent.name).unwrap_or_else(|| "No longer open".to_owned())}
                    {move || project().map(|name| format!(" · {name}"))}</p>
                <button type="button" class="tool-live-link" disabled=move || target.get().is_none()
                    on:click=move |_| {
                        if let Some(agent) = target.get_untracked() {
                            crate::components::agents_panel::open_agent_chat(&state, &agent);
                        }
                    }>"Open agent"</button>
            </div>
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
fn ChatNoticeView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    notice: ArcRwSignal<ChatNotice>,
) -> impl IntoView {
    let appearance = expect_context::<AppState>().chat_appearance;
    if notice.with_untracked(|notice| matches!(notice, ChatNotice::RetrySequence { .. })) {
        return view! { <RetrySequenceView notice=notice /> }.into_any();
    }
    view! {
        {move || match notice.get() {
            ChatNotice::RetrySequence { .. } => ().into_any(),
            ChatNotice::AgentHandoff(handoff) => view! {
                <AgentHandoffView agent_ref=agent_ref handoff=handoff />
            }.into_any(),
            ChatNotice::RestartRecovery(phase) => view! {
                <div class="chat-card chat-card-system"><div class="chat-card-body"><p>{phase.notice_text()}</p></div></div>
            }.into_any(),
            ChatNotice::GoalCompleted(goal) => view! {
                <div class="chat-card chat-card-system" data-test="goal-completed">
                    <div class="chat-card-header"><span class="chat-card-sender">"Goal completed"</span></div>
                    <div class="chat-card-body"><p>{goal.objective}</p></div>
                </div>
            }.into_any(),
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
            ChatNotice::RetryAttempt { error, attempt, max_retries, backoff_ms, .. }
                if appearance.get() == crate::appearance::ChatAppearance::Ink => view! {
                <details class="flow-retry" data-test="chat-notice-retry">
                    <summary><span aria-hidden="true" class="flow-warning-icon">"⚠"</span><span>{format!("Retry {attempt}/{max_retries}")}</span></summary>
                    <div class="flow-retry-details"><pre>{error}</pre><p>{format!("Attempt {attempt} of {max_retries} · Reported backoff: {backoff_ms}ms")}</p></div>
                </details>
            }.into_any(),
            ChatNotice::RetryAttempt { attempt, max_retries, error, backoff_ms, .. }
                if appearance.get().uses_ink_chrome() => view! {
                <details class="ink-retry" data-test="chat-notice-retry">
                    <summary>
                        <span>"Retry"</span>
                        <span>{format!("Attempt {attempt} of {max_retries}")}</span>
                        <span class="ink-retry-disclosure">"Details"</span>
                    </summary>
                    <p>{error}</p>
                    <p>{format!("Reported backoff: {backoff_ms}ms")}</p>
                </details>
            }.into_any(),
            ChatNotice::RetryAttempt { attempt, max_retries, error, backoff_ms, .. } => view! {
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
    }.into_any()
}

#[component]
fn RetrySequenceView(notice: ArcRwSignal<ChatNotice>) -> impl IntoView {
    let sequence = {
        let notice = notice.clone();
        move || {
            notice.with(|notice| match notice {
                ChatNotice::RetrySequence { sequence, .. } => Some(sequence.clone()),
                _ => None,
            })
        }
    };
    let summary = {
        let sequence = sequence.clone();
        move || {
            sequence().map(|sequence| match sequence.outcome {
                protocol::RetrySequenceOutcome::Reconnecting => {
                    match (sequence.attempt, sequence.max_attempts) {
                        (Some(attempt), Some(max)) => format!("Reconnecting… {attempt}/{max}"),
                        _ => "Reconnecting…".to_owned(),
                    }
                }
                protocol::RetrySequenceOutcome::Reconnected => "Reconnected".to_owned(),
                protocol::RetrySequenceOutcome::Failed => "Connection failed".to_owned(),
                protocol::RetrySequenceOutcome::Unresolved => "Connection unresolved".to_owned(),
            })
        }
    };
    let recovered = Signal::derive({
        let sequence = sequence.clone();
        move || {
            sequence().is_some_and(|sequence| {
                sequence.outcome == protocol::RetrySequenceOutcome::Reconnected
            })
        }
    });
    let open_notice = notice.clone();
    view! {
            <details class="flow-retry" data-test="retry-sequence"
                open=move || notice.with(|notice| matches!(notice, ChatNotice::RetrySequence { expanded: true, .. }))
                on:toggle=move |event| {
                    let element = event_target::<web_sys::HtmlDetailsElement>(&event);
                    open_notice.update(|notice| {
                        if let ChatNotice::RetrySequence { expanded, .. } = notice { *expanded = element.open(); }
                    });
                }>
                <summary><span aria-hidden="true" class="flow-warning-icon" data-recovered=move || recovered.get().to_string()>{move || if recovered.get() { "✓" } else { "⚠" }}</span><span>{summary}</span></summary>
                <div class="flow-retry-details">
                    {move || sequence().map(|sequence| sequence.diagnostics.into_iter().map(|diagnostic| view! { <pre>{diagnostic.raw}</pre> }).collect_view())}
                </div>
            </details>
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

#[component]
pub(crate) fn TydePermissionsIcon(
    permissions: Signal<protocol::AgentTydePermissions>,
) -> impl IntoView {
    view! {
        <svg width="16" height="16" viewBox="0 0 20 20" fill="none" stroke="currentColor"
            stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d=move || match permissions.get() {
                protocol::AgentTydePermissions::Agent => "M8 2h4v4H8z M2 14h4v4H2z M14 14h4v4h-4z M10 6v4H4v4 M10 10h6v4",
                protocol::AgentTydePermissions::Orchestrator => "M8 8h4v4H8z M8 1h4v3H8z M1 14h3v3H1z M16 14h3v3h-3z M10 4v4 M8 12l-4 2 M12 12l4 2",
                protocol::AgentTydePermissions::Root => "M8 2h4l1 3 3 1 2 3-2 3-3 1-1 3H8l-1-3-3-1-2-3 2-3 3-1z M10 7a3 3 0 1 0 0 6 3 3 0 0 0 0-6",
            } />
        </svg>
    }
}

#[component]
fn AgentTydePermissionsMenu(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let agents = state.agents;
    let agent = Memo::new(move |_| {
        let target = agent_ref.get()?;
        agents.with(|agents| {
            agents
                .iter()
                .find(|agent| agent.host_id == target.host_id && agent.agent_id == target.agent_id)
                .cloned()
        })
    });
    let permissions = Signal::derive(move || {
        agent
            .get()
            .map(|agent| agent.tyde_permissions)
            .unwrap_or_default()
    });
    let open = RwSignal::new(false);
    let target = Memo::new(move |_| agent_ref.get());
    Effect::new(move || {
        // Read the lazy memo so it subscribes to pane identity changes.
        target.with(|_| open.set(false));
    });
    view! {
        <Show when=move || agent.get().is_some_and(|agent| agent.origin != protocol::AgentOrigin::BackendNative)>
            <div class="chat-tyde-permissions" on:keydown=move |event: web_sys::KeyboardEvent| {
                if event.key() == "Escape" { open.set(false); event.stop_propagation(); }
            }>
                <button type="button" class="chat-tyde-permissions-toggle" aria-label="Tyde permissions"
                    aria-haspopup="menu" aria-expanded=move || open.get().to_string()
                    title=move || format!("Tyde permissions: {}", permissions.get().label())
                    disabled=move || agent.get().is_none_or(|agent| !agent.started || agent.fatal_error.is_some())
                    on:click=move |_| open.update(|value| *value = !*value)>
                    <TydePermissionsIcon permissions=permissions />
                </button>
                <Show when=move || open.get()>
                    <button type="button" class="tyde-permissions-dismiss" aria-label="Close Tyde permissions"
                        tabindex="-1" on:click=move |_| open.set(false) />
                    <div class="tyde-permissions-menu" role="menu" aria-label="Tyde permissions">
                        <div class="tyde-permissions-heading">"Tyde permissions"</div>
                        {[protocol::AgentTydePermissions::Agent, protocol::AgentTydePermissions::Orchestrator, protocol::AgentTydePermissions::Root]
                            .into_iter().map(|access| view! {
                                <button type="button" role="menuitemradio" aria-checked=move || (permissions.get() == access).to_string()
                                    on:click=move |_| {
                                        let Some(agent) = agent.get_untracked() else { return; };
                                        open.set(false);
                                        wasm_bindgen_futures::spawn_local(async move {
                                            if let Err(error) = send_frame(&agent.host_id, agent.instance_stream, protocol::FrameKind::SetAgentTydePermissions,
                                                &protocol::SetAgentTydePermissionsPayload { access }).await {
                                                crate::notices::report_agent_error(&ActiveAgentRef { host_id: agent.host_id, agent_id: agent.agent_id }, &error);
                                            }
                                        });
                                    }>
                                    <TydePermissionsIcon permissions=Signal::derive(move || access) />
                                    <span class="tyde-permissions-copy"><span>{access.label()}</span><small>{access.control_description()}</small></span>
                                    <span class="tyde-permissions-check" aria-hidden="true">{move || if permissions.get() == access { "✓" } else { "" }}</span>
                                </button>
                            }).collect_view()}
                    </div>
                </Show>
            </div>
        </Show>
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

    #[wasm_bindgen::prelude::wasm_bindgen(inline_js = "
        export async function settle_reply_cards(root, phase, expectedText) {
            console.log('Reply layout awaiting DOM', phase);
            await new Promise(resolve => {
                const ready = () => root.textContent.includes(expectedText)
                    && root.querySelectorAll('.chat-card-assistant').length === 2
                    && (phase !== 'completed' || !root.querySelector('.chat-card-streaming'));
                const observer = new MutationObserver(() => {
                    if (ready()) {
                        observer.disconnect();
                        resolve();
                    }
                });
                observer.observe(root, { childList: true, subtree: true, characterData: true, attributes: true });
                if (ready()) {
                    observer.disconnect();
                    resolve();
                }
            });
            const cards = [...root.querySelectorAll('.chat-card-assistant')];
            const animations = cards.flatMap(card => {
                const style = getComputedStyle(card);
                const rect = card.getBoundingClientRect();
                const row = card.parentElement.getBoundingClientRect();
                const entering = card.getAnimations().filter(animation => animation.animationName === 'card-enter');
                console.log('Reply layout', phase, JSON.stringify({
                    top: rect.top, bottom: rect.bottom, rowTop: row.top, rowBottom: row.bottom,
                    transform: style.transform,
                    animations: entering.map(animation => ({ state: animation.playState, time: animation.currentTime }))
                }));
                return entering;
            });
            await Promise.all(animations.map(animation => animation.finished));
        }
    ")]
    extern "C" {
        #[wasm_bindgen::prelude::wasm_bindgen(catch)]
        async fn settle_reply_cards(
            root: &web_sys::HtmlElement,
            phase: &str,
            expected_text: &str,
        ) -> Result<JsValue, JsValue>;
    }

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

    #[wasm_bindgen_test]
    async fn flow_retry_sequence_updates_in_place_and_replays_without_losing_details() {
        use crate::appearance::ChatAppearance;
        use protocol::{RetryDiagnostic, RetrySequence, RetrySequenceId, RetrySequenceOutcome};
        ensure_styles_loaded();
        let container = make_container();
        let style = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .create_element("style")
            .unwrap();
        style.set_text_content(Some(include_str!("../../ink-chat.css")));
        container.append_child(&style).unwrap();
        let state = AppState::new();
        state.chat_appearance.set(ChatAppearance::Ink);
        let bound = ActiveAgentRef {
            host_id: "retry-host".to_owned(),
            agent_id: AgentId("retry-agent".to_owned()),
        };
        let mounted_bound = bound.clone();
        let mounted_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! { <ChatView tab_id=TabId(20_099) agent_ref=Signal::derive(move || Some(mounted_bound.clone())) is_active=Signal::derive(|| true) /> }
        });
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &bound.agent_id, event)
        };
        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Continue the request").message,
        ));
        let mut sequence = RetrySequence {
            id: RetrySequenceId("episode-one".to_owned()),
            request: protocol::ModelTurnId("request-one".to_owned()),
            revision: 1,
            attempt: Some(1),
            max_attempts: Some(5),
            outcome: RetrySequenceOutcome::Reconnecting,
            diagnostics: Vec::new(),
        };
        for attempt in 1..=3 {
            let id = protocol::ChatMessageId(format!("diagnostic-{attempt}"));
            let mut warning = mk_user_msg("Native connection diagnostic").message;
            warning.sender = MessageSender::Warning;
            warning.message_id = Some(id.clone());
            let source = if attempt == 2 {
                let id = protocol::RetryAttemptId("native-attempt-two".to_owned());
                dispatch(ChatEvent::RetryAttempt(protocol::RetryAttemptData {
                    id: Some(id.clone()),
                    attempt: 2,
                    max_retries: 5,
                    error: "Native retry diagnostic".to_owned(),
                    backoff_ms: 250,
                }));
                protocol::RetryDiagnosticSource::Attempt(id)
            } else {
                dispatch(ChatEvent::MessageAdded(warning));
                protocol::RetryDiagnosticSource::Message(id)
            };
            sequence.revision = attempt;
            sequence.attempt = Some(attempt);
            sequence.diagnostics.push(RetryDiagnostic {
                source,
                raw: format!(
                    "native attempt {attempt}: {}",
                    "raw transport detail ".repeat(12)
                ),
            });
            dispatch(ChatEvent::RetrySequenceUpdated(sequence.clone()));
            next_tick().await;
        }
        let details = container
            .query_selector("[data-test='retry-sequence']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .unwrap();
        assert_eq!(
            container
                .query_selector_all("[data-test='retry-sequence']")
                .unwrap()
                .length(),
            1
        );
        assert!(
            details
                .query_selector("summary")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Reconnecting… 3/5")
        );
        details.set_open(true);
        details
            .dispatch_event(&web_sys::Event::new("toggle").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(details.query_selector_all("pre").unwrap().length(), 3);
        sequence.revision += 1;
        sequence.outcome = RetrySequenceOutcome::Reconnected;
        dispatch(ChatEvent::RetrySequenceUpdated(sequence.clone()));
        next_tick().await;
        let updated = container
            .query_selector("[data-test='retry-sequence']")
            .unwrap()
            .unwrap();
        assert!(
            updated.is_same_node(Some(details.as_ref())),
            "live progress preserves the mounted disclosure"
        );
        assert!(details.open());
        assert!(
            details
                .query_selector("summary")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Reconnected")
        );
        let mut old = sequence.clone();
        old.revision = 1;
        old.outcome = RetrySequenceOutcome::Reconnecting;
        old.diagnostics.truncate(1);
        state.prepend_history_page(
            &bound.agent_id,
            vec![ChatRowHandle::notice(ChatNotice::RetrySequence {
                sequence: old,
                expanded: false,
            })],
        );
        dispatch(ChatEvent::RetrySequenceUpdated(sequence.clone()));
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-test='retry-sequence']")
                .unwrap()
                .length(),
            1
        );
        assert!(details.open());
        assert_eq!(details.query_selector_all("pre").unwrap().length(), 3);
        assert!(
            details
                .query_selector("summary")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Reconnected")
        );
        sequence.id = RetrySequenceId("episode-two".to_owned());
        sequence.outcome = RetrySequenceOutcome::Failed;
        let id = protocol::ChatMessageId("other-diagnostic".to_owned());
        let mut error = mk_user_msg("Native terminal failure").message;
        error.sender = MessageSender::Error;
        error.message_id = Some(id.clone());
        dispatch(ChatEvent::MessageAdded(error));
        sequence.diagnostics = vec![RetryDiagnostic {
            source: protocol::RetryDiagnosticSource::Message(id),
            raw: "independent terminal diagnostic".to_owned(),
        }];
        dispatch(ChatEvent::RetrySequenceUpdated(sequence.clone()));
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-test='retry-sequence']")
                .unwrap()
                .length(),
            2,
            "separate episodes never coalesce even within one request"
        );
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Connection failed")
        );
        for appearance in [ChatAppearance::Current, ChatAppearance::Notebook] {
            state.chat_appearance.set(appearance);
            next_tick().await;
            assert_eq!(
                container
                    .query_selector_all("[data-test='retry-sequence']")
                    .unwrap()
                    .length(),
                0
            );
            assert_eq!(
                container
                    .query_selector_all(".chat-card-warning")
                    .unwrap()
                    .length(),
                2,
                "other appearances retain every original warning card"
            );
            assert_eq!(
                container
                    .query_selector_all("[data-test='chat-notice-retry']")
                    .unwrap()
                    .length(),
                1,
                "other appearances retain the original retry event kind"
            );
        }
        state.chat_appearance.set(ChatAppearance::Ink);
        container.style().set_property("width", "320px").unwrap();
        next_tick().await;
        next_animation_frame().await;
        let disclosure = container
            .query_selector("[data-test='retry-sequence']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .unwrap();
        assert!(disclosure.open(), "expansion survives appearance changes");
        assert!(
            disclosure.scroll_width() <= disclosure.client_width(),
            "raw details wrap at narrow widths"
        );
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn typed_home_handoff_opens_independent_owner_without_moving_help() {
        use crate::state::{ActiveProjectRef, AgentInfo, ProjectInfo};
        ensure_styles_loaded();
        let container = make_container();
        let state = AppState::new();
        let help = AgentInfo {
            tyde_permissions: protocol::AgentTydePermissions::Root,
            host_id: "home-host".to_owned(),
            agent_id: AgentId("help".to_owned()),
            name: "Help".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: Some(protocol::CustomAgentId("tyde-help".to_owned())),
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath("/agent/help".to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
            review_recipient: false,
        };
        let project_id = ProjectId("approved".to_owned());
        let mut owner = help.clone();
        owner.agent_id = AgentId("independent-owner".to_owned());
        owner.name = "Project Manager".to_owned();
        owner.tyde_permissions = protocol::AgentTydePermissions::Agent;
        owner.project_id = Some(project_id.clone());
        owner.custom_agent_id = Some(protocol::CustomAgentId("tyde-project-manager".to_owned()));
        owner.instance_stream = StreamPath("/agent/independent-owner".to_owned());
        state.agents.set(vec![help.clone(), owner.clone()]);
        state.projects.set(vec![ProjectInfo {
            host_id: "home-host".to_owned(),
            project: protocol::Project {
                id: project_id.clone(),
                name: "Approved app".to_owned(),
                source: protocol::ProjectSource::Standalone {
                    roots: vec![ProjectRootPath("/repo/app".to_owned())],
                },
                sort_order: 0,
            },
        }]);
        let bound = ActiveAgentRef {
            host_id: help.host_id.clone(),
            agent_id: help.agent_id.clone(),
        };
        crate::components::agents_panel::open_agent_chat(&state, &help);
        let active_state = state.clone();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            view! { <ChatView tab_id=TabId(10_007) agent_ref=Signal::derive(move || active_state.active_agent.get())
            is_active=Signal::derive(|| true) /> }
        });
        let mut prose = mk_user_msg("Open agent in Approved app").message;
        prose.sender = MessageSender::Assistant {
            agent: "claude".to_owned(),
        };
        crate::dispatch::apply_chat_event(
            &state,
            &bound.host_id,
            &bound.agent_id,
            ChatEvent::MessageAdded(prose),
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Independent agent:"),
            "Transcript prose must not create a handoff"
        );
        let event = ChatEvent::AgentHandoff(protocol::AgentHandoff {
            agent_id: owner.agent_id.clone(),
            project_id: owner.project_id.clone(),
        });
        crate::dispatch::apply_chat_event(&state, &bound.host_id, &bound.agent_id, event);
        next_tick().await;
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Independent agent: Project Manager · Approved app")
        );
        let buttons = container.query_selector_all("button").unwrap();
        let open = (0..buttons.length())
            .filter_map(|i| buttons.item(i))
            .filter_map(|node| node.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().as_deref() == Some("Open agent"))
            .unwrap();
        assert!(open.get_bounding_client_rect().width() > 0.0);
        let mut owner_message = mk_user_msg("The project owner retains the approved task").message;
        owner_message.sender = MessageSender::Assistant {
            agent: "claude".to_owned(),
        };
        crate::dispatch::apply_chat_event(
            &state,
            &owner.host_id,
            &owner.agent_id,
            ChatEvent::MessageAdded(owner_message),
        );
        open.click();
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("The project owner retains the approved task"),
            "Open agent must render the independent owner's conversation"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Independent agent:")
        );
        assert_eq!(
            state.active_project_ref_untracked(),
            Some(ActiveProjectRef {
                host_id: "home-host".to_owned(),
                project_id
            })
        );
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id,
            owner.agent_id
        );
        assert!(
            state
                .agents
                .get_untracked()
                .iter()
                .any(|agent| agent.agent_id == help.agent_id
                    && agent.project_id.is_none()
                    && agent.tyde_permissions == protocol::AgentTydePermissions::Root)
        );
        crate::components::agents_panel::open_agent_chat(&state, &help);
        next_tick().await;
        assert_eq!(state.active_project_ref_untracked(), None);
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Independent agent: Project Manager · Approved app"),
            "Help's Home conversation remains available after opening its owner"
        );
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id,
            help.agent_id
        );
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn restart_recovery_renders_ordered_notices_without_user_bubbles() {
        use protocol::{RestartInterruptionCause as Cause, RestartRecoveryPhase as Phase};
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-restart-recovery".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-restart-recovery".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <ChatView
                    tab_id=TabId(10_006)
                    agent_ref=Signal::derive(move || Some(bound_for_mount.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        let assistant = |text: &str| {
            let mut message = mk_user_msg(text).message;
            message.sender = MessageSender::Assistant {
                agent: "claude".to_owned(),
            };
            message
        };

        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Fix the failing build").message,
        ));
        dispatch(ChatEvent::MessageAdded(assistant("Reading the build log")));
        dispatch(ChatEvent::RestartRecovery {
            phase: Phase::Interrupted {
                cause: Cause::HostRestart,
            },
        });
        dispatch(ChatEvent::RestartRecovery {
            phase: Phase::Continuing,
        });
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: None,
        }));
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: "The build is fixed.".to_owned(),
        }));
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: assistant("The build is fixed."),
        }));
        dispatch(ChatEvent::RestartRecovery {
            phase: Phase::Interrupted {
                cause: Cause::UnexpectedStop,
            },
        });
        dispatch(ChatEvent::RestartRecovery {
            phase: Phase::ContinuationFailed {
                message: "Backend unavailable".to_owned(),
            },
        });
        next_tick().await;
        next_animation_frame().await;

        let rows = message_rows(&container);
        let texts = rows
            .iter()
            .map(|row| row.text_content().unwrap_or_default())
            .collect::<Vec<_>>();
        console_log!("Restart recovery transcript rows: {texts:?}");
        assert_eq!(
            rows.len(),
            7,
            "each message and each server recovery phase is exactly one row: {texts:?}"
        );
        assert!(texts[0].contains("Fix the failing build"), "{texts:?}");
        assert!(texts[1].contains("Reading the build log"), "{texts:?}");
        assert_eq!(
            texts[2].trim(),
            "This turn was interrupted because the host restarted."
        );
        assert_eq!(
            texts[3].trim(),
            "Continuing the interrupted turn after restart."
        );
        assert!(texts[4].contains("The build is fixed."), "{texts:?}");
        assert_eq!(
            texts[5].trim(),
            "This turn was interrupted because the host stopped unexpectedly."
        );
        assert_eq!(
            texts[6].trim(),
            "Could not continue the interrupted turn: Backend unavailable"
        );

        let senders = rows
            .iter()
            .map(|row| {
                row.query_selector(".chat-card-sender")
                    .unwrap()
                    .and_then(|label| label.text_content())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            senders
                .iter()
                .filter(|sender| sender.trim() == "You")
                .count(),
            1,
            "only the real user message renders as a user bubble; the server-composed \
             continuation must never appear as one: {senders:?}"
        );
        assert_eq!(senders[0].trim(), "You");
        for index in [2, 3, 5, 6] {
            assert!(
                senders[index].is_empty(),
                "recovery notice row {index} carries no sender label: {senders:?}"
            );
        }
        drop(handle);
        container.remove();
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
                origin: None,
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
    async fn streaming_reply_keeps_its_gap_when_completed() {
        ensure_styles_loaded();
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-stream-spacing".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-spacing".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let bound = bound_for_mount.clone();
            view! {
                <ChatView
                    tab_id=TabId(10_004)
                    agent_ref=Signal::derive(move || Some(bound.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        let mut previous = mk_user_msg("Previous assistant reply").message;
        previous.sender = MessageSender::Assistant {
            agent: "claude".to_owned(),
        };
        dispatch(ChatEvent::MessageAdded(previous));
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: Some("claude-test".to_owned()),
        }));

        async fn settle_layout(container: &HtmlElement, phase: &str, expected_text: &str) {
            leptos::task::tick().await;
            settle_reply_cards(container, phase, expected_text)
                .await
                .expect("reply entrance animations complete");
        }

        let measure_gap = |phase: &str| {
            let cards = container
                .query_selector_all(".chat-card-assistant")
                .unwrap();
            assert_eq!(cards.length(), 2, "{phase}: both replies must be visible");
            let previous = cards.item(0).unwrap().dyn_into::<Element>().unwrap();
            let current = cards.item(1).unwrap().dyn_into::<Element>().unwrap();
            let gap = current.get_bounding_client_rect().top()
                - previous.get_bounding_client_rect().bottom();
            console_log!("Reply spacing during {phase}: {gap}px");
            assert!(
                (gap - ROW_GAP_PX).abs() < 0.5,
                "{phase}: replies must retain the normal {ROW_GAP_PX}px gap, got {gap}px"
            );
            gap
        };

        settle_layout(&container, "thinking", "Thinking\u{2026}").await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Thinking\u{2026}")
        );
        let thinking_gap = measure_gap("thinking");
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: "The reply is arriving".to_owned(),
        }));
        settle_layout(&container, "streaming", "The reply is arriving").await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("The reply is arriving")
        );
        let streaming_gap = measure_gap("streaming");
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: " and is now complete.".to_owned(),
        }));
        settle_layout(
            &container,
            "last delta",
            "The reply is arriving and is now complete.",
        )
        .await;
        measure_gap("last delta");
        let mut completed = mk_user_msg("The reply is arriving and is now complete.").message;
        completed.sender = MessageSender::Assistant {
            agent: "claude".to_owned(),
        };
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: completed,
        }));
        settle_layout(
            &container,
            "completed",
            "The reply is arriving and is now complete.",
        )
        .await;
        assert!(query(&container, ".chat-card-streaming").is_none());
        let completed_gap = measure_gap("completed");
        assert!((thinking_gap - completed_gap).abs() < 0.5);
        assert!((streaming_gap - completed_gap).abs() < 0.5);
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn ink_keeps_actions_visible_and_expands_only_their_output() {
        use crate::appearance::ChatAppearance;
        use crate::state::ToolOutputMode;
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-ink-stretch".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-ink-stretch".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <ChatView
                    tab_id=TabId(10_041)
                    agent_ref=Signal::derive(move || Some(bound_for_mount.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        state.chat_appearance.set(ChatAppearance::Ink);
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        let assistant = |text: &str| {
            let mut message = mk_user_msg(text).message;
            message.sender = MessageSender::Assistant {
                agent: "claude".to_owned(),
            };
            message
        };

        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Inspect the workspace").message,
        ));
        async fn settle() {
            next_tick().await;
            next_animation_frame().await;
            next_animation_frame().await;
        }
        // Measure before streaming scrolls this row out of the virtualized viewport.
        for width in [960, 360] {
            container
                .style()
                .set_property("width", &format!("{width}px"))
                .unwrap();
            settle().await;
            let user = container
                .query_selector(".chat-card-user")
                .unwrap()
                .unwrap();
            let sender = user
                .query_selector(".chat-card-sender")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect();
            let body = user
                .query_selector(".chat-card-body")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect();
            assert!(
                (sender.left() - body.left()).abs() < 1.0,
                "short user text starts beneath its sender at wide and narrow widths"
            );
        }
        container.style().set_property("width", "800px").unwrap();
        settle().await;
        let commands = [
            "python3 /workspace/a-long-directory-name/another-long-directory-name/first.py --description 'Keep the actual command readable without a large wrapping header'",
            "python3 second.py",
            "python3 third.py",
        ];
        for (index, command) in commands.into_iter().enumerate() {
            let text = if index == 0 {
                "I'll inspect the workspace."
            } else {
                ""
            };
            let tool_call_id = format!("ink-stretch-{index}");
            dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
                agent: "claude".to_owned(),
                model: Some("claude-test".to_owned()),
            }));
            if !text.is_empty() {
                dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
                    text: text.to_owned(),
                }));
            }
            dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
                tool_call_id: tool_call_id.clone(),
                tool_name: "Bash".to_owned(),
                tool_type: protocol::ToolRequestType::RunCommand {
                    command: command.to_owned(),
                    working_directory: "/workspace".to_owned(),
                },
            }));
            if index == 0 {
                settle().await;
                for mode in [
                    ToolOutputMode::Summary,
                    ToolOutputMode::Full,
                    ToolOutputMode::Compact,
                ] {
                    state.tool_output_mode.set(mode);
                    settle().await;
                    let card = container
                        .query_selector("details.tool-card")
                        .unwrap()
                        .unwrap()
                        .dyn_into::<web_sys::HtmlDetailsElement>()
                        .unwrap();
                    assert_eq!(
                        card.open(),
                        mode == ToolOutputMode::Full,
                        "running commands obey output-mode defaults"
                    );
                    assert!(card.text_content().unwrap().contains("Running"));
                    let style = web_sys::window()
                        .unwrap()
                        .get_computed_style(&card)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        style.get_property_value("border-top-width").unwrap(),
                        "0px",
                        "running commands do not gain a heavy frame"
                    );
                }
            }
            dispatch(ChatEvent::ToolExecutionCompleted(
                protocol::ToolExecutionCompletedData {
                    tool_call_id: tool_call_id.clone(),
                    outcome: if index == 1 {
                        protocol::ToolExecutionOutcome::Failed {
                            message: "Probe failed".to_owned(),
                            details: None,
                            normalization_failure: None,
                            result: Some(protocol::ToolExecutionResult::NativeContent {
                                text: "Native failure output".to_owned(),
                                images: Vec::new(),
                            }),
                        }
                    } else {
                        protocol::ToolExecutionOutcome::Succeeded {
                            result: protocol::ToolExecutionResult::RunCommand {
                                exit_code: 0,
                                stdout: "ok\n".to_owned(),
                                stderr: String::new(),
                            },
                        }
                    },
                },
            ));
            let mut message = assistant(text);
            if index == 0 {
                message.reasoning = Some(protocol::ReasoningData {
                    text: "Weighing the results.".to_owned(),
                    tokens: Some(12),
                    signature: None,
                    blob: None,
                });
            }

            message.tool_calls.push(protocol::ToolUseData {
                tool_call_id,
                name: "Bash".to_owned(),
                arguments: serde_json::json!({ "command": command }),
                content_offset: None,
            });
            dispatch(ChatEvent::StreamEnd(protocol::StreamEndData { message }));
        }
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: Some("claude-test".to_owned()),
        }));
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: "All done.".to_owned(),
        }));
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: assistant("All done."),
        }));

        let text_of = |element: &Element| element.text_content().unwrap_or_default();
        let visible = |selector: &str| {
            let nodes = container.query_selector_all(selector).unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
                .filter(|element| element.get_bounding_client_rect().height() > 0.0)
                .collect::<Vec<_>>()
        };
        let bylines = || {
            visible(".chat-card-assistant > .chat-card-footer")
                .into_iter()
                .filter(|footer| {
                    web_sys::window()
                        .unwrap()
                        .get_computed_style(footer)
                        .unwrap()
                        .unwrap()
                        .get_property_value("opacity")
                        .unwrap()
                        != "0"
                })
                .count()
        };
        let command_headers = || {
            let headers = visible(".tool-card-header");
            commands
                .iter()
                .filter_map(|command| {
                    headers
                        .iter()
                        .find(|header| text_of(header).contains(command))
                        .cloned()
                })
                .collect::<Vec<_>>()
        };
        // The user explicitly replaced the old folded-action contract:
        // Summary hides output, never the action chronology itself.
        settle().await;
        assert!(
            visible(".chat-card-reasoning").is_empty(),
            "Flow hides reasoning disclosures by default when Show reasoning is off"
        );
        state.flow_show_reasoning.set(true);
        settle().await;
        assert_eq!(
            visible(".chat-card-reasoning").len(),
            1,
            "enabling Show reasoning reveals the compact disclosure beside visible actions"
        );
        {
            let reasoning_summary = container
                .query_selector(".chat-card-reasoning summary")
                .unwrap()
                .unwrap();
            let sender = container
                .query_selector(".token-stat-agent")
                .unwrap()
                .unwrap();
            let px_of = |el: &Element| -> f64 {
                web_sys::window()
                    .unwrap()
                    .get_computed_style(el)
                    .unwrap()
                    .unwrap()
                    .get_property_value("font-size")
                    .unwrap()
                    .trim_end_matches("px")
                    .parse()
                    .unwrap()
            };
            assert!(
                px_of(&reasoning_summary) < px_of(&sender),
                "Flow reasoning summary ({}px) stays smaller than sender label ({}px)",
                px_of(&reasoning_summary),
                px_of(&sender)
            );
        }
        state.flow_show_reasoning.set(false);
        settle().await;
        assert!(
            visible(".chat-card-reasoning").is_empty(),
            "disabling Show reasoning hides Flow reasoning again while keeping text and tools"
        );
        assert_eq!(bylines(), 1, "the stretch carries a single byline");
        let headers = command_headers();
        assert_eq!(
            headers.len(),
            3,
            "Compact shows every action without a group disclosure"
        );
        let cards: Vec<web_sys::HtmlDetailsElement> = headers
            .iter()
            .map(|header| header.parent_element().unwrap().dyn_into().unwrap())
            .collect();
        assert!(
            cards.iter().all(|card| !card.open()),
            "Compact hides output by default"
        );
        assert!(
            headers[1]
                .text_content()
                .unwrap_or_default()
                .contains("Failed"),
            "failed actions remain readable between successes"
        );
        assert!(
            headers
                .windows(2)
                .all(|pair| pair[0].get_bounding_client_rect().top()
                    < pair[1].get_bounding_client_rect().top()),
            "actions stay chronological"
        );

        // A narrow container does not exercise viewport media queries. Adopt
        // the actual mounted card into an iframe with the production styles.
        let document = web_sys::window().unwrap().document().unwrap();
        let frame = document
            .create_element("iframe")
            .unwrap()
            .dyn_into::<web_sys::HtmlIFrameElement>()
            .unwrap();
        frame
            .set_attribute("style", "width:360px;height:240px;border:0")
            .unwrap();
        let loaded = js_sys::Function::new_with_args(
            "frame",
            "return new Promise(resolve => frame.addEventListener('load', resolve, {once:true}));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &frame)
        .unwrap();
        let ink_styles = include_str!("../../ink-chat.css");
        frame.set_attribute("srcdoc", &format!(
            "<style>{PROD_STYLES}{ink_styles}</style><div class='chat-view' data-chat-chrome='ink' data-chat-appearance='ink'><div class='chat-card-tools' id='activity'></div></div>"
        )).unwrap();
        document.body().unwrap().append_child(&frame).unwrap();
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(loaded))
            .await
            .unwrap();
        // Child-realm Document/Window fail parent-realm instanceof checks.
        let frame_document = frame.content_document().unwrap();
        let frame_window = frame.content_window().unwrap();
        let parent = cards[1].parent_node().unwrap();
        let next = cards[1].next_sibling();
        frame_document
            .get_element_by_id("activity")
            .unwrap()
            .append_child(&cards[1])
            .unwrap();
        settle().await;
        let failure = cards[1]
            .query_selector(".tool-status-text.failure")
            .unwrap()
            .unwrap();
        let summary = cards[1]
            .query_selector(".tool-completion-summary")
            .unwrap()
            .unwrap();
        assert_eq!(
            frame_window
                .get_computed_style(&summary)
                .unwrap()
                .unwrap()
                .get_property_value("display")
                .unwrap(),
            "none"
        );
        assert_eq!(failure.text_content().as_deref(), Some("Failed"));
        for expanded in [false, true] {
            cards[1].set_open(expanded);
            settle().await;
            assert!(
                failure.get_bounding_client_rect().width() > 10.0
                    && failure.get_bounding_client_rect().height() > 5.0,
                "narrow failed tools retain a readable status with output closed or open"
            );
        }
        cards[1].set_open(false);
        parent.insert_before(&cards[1], next.as_ref()).unwrap();
        frame.remove();
        settle().await;

        state.tool_output_mode.set(ToolOutputMode::Summary);
        settle().await;
        assert_eq!(
            command_headers().len(),
            3,
            "Summary still shows all actions"
        );
        assert!(cards.iter().all(|card| !card.open()));
        headers[0].dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        assert!(cards[0].open(), "Summary permits explicit output expansion");
        let stdout = cards[0]
            .query_selector(".tool-result-stdout")
            .unwrap()
            .expect("explicit expansion renders stdout");
        assert_eq!(
            stdout.text_content().unwrap().trim(),
            "ok",
            "Summary reveals the command output only when requested"
        );
        assert!(stdout.get_bounding_client_rect().height() > 0.0);
        let literal = cards[0]
            .query_selector(".tool-command-line")
            .unwrap()
            .unwrap();
        assert_eq!(literal.text_content().as_deref(), Some(commands[0]));
        assert!(
            literal.get_bounding_client_rect().height() > 0.0,
            "expanded details show the selectable literal command, not only the truncated header"
        );
        assert!(
            literal.get_bounding_client_rect().bottom() <= stdout.get_bounding_client_rect().top()
        );
        assert!(
            cards[0].get_bounding_client_rect().height() < 180.0,
            "short output has no forced empty panel"
        );
        assert_eq!(
            visible(".tool-card-header")[0]
                .get_bounding_client_rect()
                .height(),
            visible(".tool-card-header")[1]
                .get_bounding_client_rect()
                .height(),
            "success and failure share the same compact line"
        );
        assert!(!cards[1].open());
        headers[1].dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        let failed_request = cards[1]
            .query_selector(".tool-command-line")
            .unwrap()
            .expect("Summary exposes an explicitly opened native failure's request");
        assert_eq!(failed_request.text_content().as_deref(), Some(commands[1]));
        assert!(failed_request.get_bounding_client_rect().height() > 0.0);
        headers[1].dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;

        state.tool_output_mode.set(ToolOutputMode::Full);
        settle().await;
        assert_eq!(command_headers().len(), 3);
        assert!(
            cards.iter().all(|card| card.open()),
            "Full opens all outputs"
        );
        let failed_output = cards[1].query_selector(".tool-error-pre").unwrap().unwrap();
        assert_eq!(
            failed_output.text_content().as_deref(),
            Some("Probe failed")
        );
        let failed_command = cards[1]
            .query_selector(".tool-command-line")
            .unwrap()
            .unwrap();
        assert_eq!(failed_command.text_content().as_deref(), Some(commands[1]));
        assert!(
            failed_command.get_bounding_client_rect().height() > 0.0,
            "failed command details keep the full literal request accessible"
        );
        assert!(
            failed_output.get_bounding_client_rect().height() > 0.0,
            "genuine failure remains inspectable"
        );
        let error_style = web_sys::window()
            .unwrap()
            .get_computed_style(&failed_output)
            .unwrap()
            .unwrap();
        assert_eq!(
            error_style.get_property_value("border-top-width").unwrap(),
            "0px",
            "failure details stay unframed"
        );
        assert_eq!(
            error_style.get_property_value("background-color").unwrap(),
            "rgba(0, 0, 0, 0)"
        );
        state.tool_output_mode.set(ToolOutputMode::Compact);
        settle().await;
        assert!(cards.iter().all(|card| !card.open()));
        headers[0].dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        assert!(cards[0].open());
        state.chat_appearance.set(ChatAppearance::Current);
        settle().await;
        assert_eq!(command_headers().len(), 3, "Current shows every tool card");
        assert_eq!(bylines(), 4, "Current keeps a byline per reply");
        assert_eq!(
            visible(".chat-card-reasoning").len(),
            1,
            "Default presentation still renders reasoning when Flow Show reasoning is off"
        );
        state.chat_appearance.set(ChatAppearance::Notebook);
        settle().await;
        assert_eq!(
            visible(".chat-card-reasoning").len(),
            1,
            "Notebook presentation still renders reasoning when Flow Show reasoning is off"
        );
        state.chat_appearance.set(ChatAppearance::Ink);
        settle().await;
        assert_eq!(command_headers().len(), 3);
        assert!(
            headers[0].is_same_node(command_headers().first().map(|element| element.as_ref())),
            "appearance switches retain the tool node"
        );
        assert!(
            cards[0].open(),
            "explicit output choice survives appearance switches"
        );

        let root = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .document_element()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        let original_style = root.get_attribute("style");
        root.style()
            .set_property("--font-sans", "monospace")
            .unwrap();
        for size in [11, 13, 20] {
            root.style()
                .set_property("--base-font-size", &format!("{size}px"))
                .unwrap();
            let roles = [
                ".chat-card-body",
                ".tool-card-header",
                ".tool-card-name",
                ".chat-textarea",
            ];
            state.chat_appearance.set(ChatAppearance::Current);
            settle().await;
            let styles = |selector: &str| {
                let node = container.query_selector(selector).unwrap().unwrap();
                let style = web_sys::window()
                    .unwrap()
                    .get_computed_style(&node)
                    .unwrap()
                    .unwrap();
                (
                    style.get_property_value("font-size").unwrap(),
                    style.get_property_value("font-family").unwrap(),
                )
            };
            let current: Vec<_> = roles.iter().map(|selector| styles(selector)).collect();
            state.chat_appearance.set(ChatAppearance::Ink);
            settle().await;
            for (selector, expected) in roles.iter().zip(current) {
                assert_eq!(
                    styles(selector).0,
                    expected.0,
                    "Ink uses the configured application role size: {selector}"
                );
            }
            assert!(
                styles(".chat-card-body").1.contains("monospace"),
                "Ink honors the selected typeface"
            );
        }
        match original_style {
            Some(style) => root.set_attribute("style", &style).unwrap(),
            None => root.remove_attribute("style").unwrap(),
        }
        settle().await;

        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: Some("claude-test".to_owned()),
        }));
        let long_answer = format!("Workspace note {}", "unbroken".repeat(40));
        let option = |label: &str, description: Option<String>| protocol::AskUserQuestionOption {
            label: label.to_owned(),
            description,
        };
        dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "ink-stretch-ask".to_owned(),
            tool_name: "AskUserQuestion".to_owned(),
            tool_type: protocol::ToolRequestType::AskUserQuestion {
                mode: protocol::UserQuestionMode::Blocking,
                questions: vec![
                    protocol::AskUserQuestion {
                        id: None,
                        question: "Which workspace?".to_owned(),
                        header: Some("Workspace".to_owned()),
                        answer_schema: None,
                        multi_select: false,
                        options: vec![
                            option("Current", None),
                            option(
                                "Fresh workbench",
                                Some(format!(
                                    "Create /workbenches/{} before editing",
                                    "unbroken".repeat(30)
                                )),
                            ),
                        ],
                    },
                    protocol::AskUserQuestion {
                        id: None,
                        question: "Which checks should run before landing?".to_owned(),
                        header: Some("Checks".to_owned()),
                        answer_schema: None,
                        multi_select: true,
                        options: vec![
                            option("Format", Some("Rewrite whitespace only".to_owned())),
                            option("Lint", None),
                        ],
                    },
                ],
            },
        }));
        settle().await;
        state.tool_output_mode.set(ToolOutputMode::Summary);
        settle().await;
        let question = visible(".tool-card-header")
            .into_iter()
            .find(|header| text_of(header).contains("Awaiting answer"))
            .expect("a live pending decision stays readable alongside settled actions");
        let card = question.parent_element().expect("decision card");
        let css = |element: &Element, property: &str| {
            web_sys::window()
                .unwrap()
                .get_computed_style(element)
                .unwrap()
                .unwrap()
                .get_property_value(property)
                .unwrap()
        };
        assert_eq!(
            css(&card, "border-top-width"),
            "0px",
            "a pending question is not boxed inside the transcript"
        );
        let decision = card
            .query_selector(".ask-question-card")
            .unwrap()
            .expect("the pending question renders its choices");
        let decision_body = decision.closest(".tool-card-body").unwrap().unwrap();
        assert_eq!(
            css(&decision_body, "border-left-width"),
            "2px",
            "a pending question is marked by a single rule"
        );
        let options = |label: &str| {
            let nodes = decision.query_selector_all("button").unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
                .find(|button| {
                    button
                        .first_element_child()
                        .is_some_and(|child| text_of(&child) == label)
                })
                .unwrap_or_else(|| panic!("option {label} is offered"))
        };
        let submit = || {
            decision
                .query_selector("button.ask-question-submit")
                .unwrap()
                .unwrap()
                .dyn_into::<web_sys::HtmlButtonElement>()
                .unwrap()
        };
        let prompt_size = |selector: &str| {
            css(
                &decision.query_selector(selector).unwrap().unwrap(),
                "font-size",
            )
            .trim_end_matches("px")
            .parse::<f64>()
            .unwrap()
        };
        assert!(
            prompt_size(".ask-question-text") > prompt_size(".ask-question-header"),
            "the question reads above its header"
        );
        assert!(submit().disabled(), "nothing submits before an answer");
        options("Fresh workbench").click();
        options("Format").click();
        options("Lint").click();
        settle().await;
        assert_ne!(
            css(options("Fresh workbench").as_ref(), "background-color"),
            css(options("Current").as_ref(), "background-color"),
            "a chosen option is visibly selected"
        );
        for (label, pressed) in [
            ("Current", "false"),
            ("Fresh workbench", "true"),
            ("Format", "true"),
            ("Lint", "true"),
        ] {
            assert_eq!(
                options(label).get_attribute("aria-pressed").as_deref(),
                Some(pressed),
                "{label} reports its selection"
            );
        }
        let described = options("Fresh workbench");
        assert!(
            described.scroll_width() <= described.client_width()
                && described.get_bounding_client_rect().right()
                    <= decision.get_bounding_client_rect().right() + 0.5,
            "long option descriptions wrap inside their choice"
        );
        let answer = decision
            .query_selector("input.ask-question-custom")
            .unwrap()
            .expect("pending question accepts a custom answer")
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        answer.set_value(&long_answer);
        answer
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        settle().await;
        assert!(
            !submit().disabled(),
            "an answered decision can be submitted"
        );
        assert_eq!(submit().text_content().as_deref(), Some("Submit answer"));
        let indicator = |option: &HtmlElement| {
            web_sys::window()
                .unwrap()
                .get_computed_style_with_pseudo_elt(option, "::before")
                .unwrap()
                .unwrap()
                .get_property_value("display")
                .unwrap()
        };
        for appearance in [
            ChatAppearance::Current,
            ChatAppearance::Notebook,
            ChatAppearance::Ink,
        ] {
            state.chat_appearance.set(appearance);
            settle().await;
            let current = container
                .query_selector(".ask-question-card")
                .unwrap()
                .unwrap();
            assert!(
                decision.is_same_node(Some(&current)),
                "appearance switches do not rebuild an interactive decision"
            );
            let field = decision
                .query_selector("input.ask-question-custom")
                .unwrap()
                .unwrap();
            assert!(
                answer.is_same_node(Some(&field)),
                "appearance switches keep the same answer field"
            );
            assert_eq!(
                answer.value(),
                long_answer,
                "the draft answer survives appearance switches"
            );
            assert_eq!(
                options("Fresh workbench")
                    .get_attribute("aria-pressed")
                    .as_deref(),
                Some("true"),
                "selections survive appearance switches"
            );
            assert!(!submit().disabled(), "appearance switches never submit");
            if appearance != ChatAppearance::Current {
                assert_ne!(
                    indicator(&options("Current")),
                    "none",
                    "{appearance:?} marks an open choice while the question is pending"
                );
            }
            let messages = container.query_selector(".chat-messages").unwrap().unwrap();
            assert!(
                messages.scroll_width() <= messages.client_width()
                    && decision.get_bounding_client_rect().right()
                        <= messages.get_bounding_client_rect().right() + 0.5,
                "the question stays inside the pane"
            );
        }
        dispatch(ChatEvent::ToolExecutionCompleted(
            protocol::ToolExecutionCompletedData {
                tool_call_id: "ink-stretch-ask".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::AskUserQuestion { answers: vec![] },
                },
            },
        ));
        for appearance in [ChatAppearance::Notebook, ChatAppearance::Ink] {
            state.chat_appearance.set(appearance);
            settle().await;
            let nodes = container
                .query_selector_all(".ask-question-option")
                .unwrap();
            let settled: Vec<HtmlElement> = (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
                .filter(|option| option.get_bounding_client_rect().height() > 0.0)
                .collect();
            assert_eq!(
                settled.len(),
                4,
                "{appearance:?} keeps every settled choice readable"
            );
            for option in &settled {
                assert_eq!(
                    indicator(option),
                    "none",
                    "{appearance:?} draws no empty choice marks when the answer is unknown"
                );
                let label = option
                    .query_selector(".ask-question-option-label")
                    .unwrap()
                    .unwrap();
                assert!(
                    label.get_bounding_client_rect().left()
                        - option.get_bounding_client_rect().left()
                        < 16.0,
                    "{appearance:?} leaves no empty mark gutter beside settled choices"
                );
                assert_eq!(
                    css(option.as_ref(), "opacity"),
                    "1",
                    "{appearance:?} does not dim choices the answer may have used"
                );
            }
            let fields = settled[0]
                .closest(".ask-question-card")
                .unwrap()
                .unwrap()
                .query_selector_all(".ask-question-custom")
                .unwrap();
            assert!(fields.length() > 0);
            for i in 0..fields.length() {
                let field = fields.item(i).unwrap().dyn_into::<Element>().unwrap();
                assert_eq!(
                    field.get_bounding_client_rect().height(),
                    0.0,
                    "{appearance:?} shows no empty, editable-looking answer field once answered"
                );
            }
        }
        let mut question_message = assistant("");
        question_message.tool_calls.push(protocol::ToolUseData {
            tool_call_id: "ink-stretch-ask".to_owned(),
            name: "AskUserQuestion".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: question_message,
        }));
        dispatch(ChatEvent::RetryAttempt(protocol::RetryAttemptData {
            id: None,
            attempt: 1,
            max_retries: 3,
            error: "Transport interrupted".to_owned(),
            backoff_ms: 250,
        }));
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: Some("claude-test".to_owned()),
        }));
        dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "ink-stretch-send".to_owned(),
            tool_name: "tyde_send_agent_message".to_owned(),
            tool_type: protocol::ToolRequestType::TydeSendAgentMessage {
                agent_id: AgentId("ink-recipient".to_owned()),
                message: "Inspect the workspace and retain the current layout.\n\n".repeat(40),
                interrupt: false,
            },
        }));
        dispatch(ChatEvent::ToolExecutionCompleted(
            protocol::ToolExecutionCompletedData {
                tool_call_id: "ink-stretch-send".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::TydeSendAgentMessage,
                },
            },
        ));
        let mut sent_message = assistant("");
        sent_message.tool_calls.push(protocol::ToolUseData {
            tool_call_id: "ink-stretch-send".to_owned(),
            name: "tyde_send_agent_message".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: sent_message,
        }));
        settle().await;
        let send_header = visible(".tool-card-header")
            .into_iter()
            .find(|header| text_of(header).contains("tyde_send_agent_message"))
            .expect("the message action stays visible in Summary");
        send_header.dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        let body = container
            .query_selector(".tool-send-message-body")
            .unwrap()
            .expect("the explicitly opened message body is visible in Summary");
        let send_card = body.closest(".tool-card").unwrap().unwrap();
        let send_header = send_card.query_selector("summary").unwrap().unwrap();
        let retry = container
            .query_selector("[data-test=chat-notice-retry]")
            .unwrap()
            .unwrap();
        let current_question = container
            .query_selector(".ask-question-custom")
            .unwrap()
            .unwrap()
            .closest(".tool-card")
            .unwrap()
            .unwrap();
        let gap_before_retry = retry.get_bounding_client_rect().top()
            - current_question.get_bounding_client_rect().bottom();
        let gap_after_retry = send_header.get_bounding_client_rect().top()
            - retry.get_bounding_client_rect().bottom();
        assert!(gap_before_retry >= 0.0 && gap_after_retry >= 0.0);
        assert!(
            gap_after_retry <= gap_before_retry,
            "a retry does not create a turn-sized gap inside the same stretch: \
             before={gap_before_retry}px, after={gap_after_retry}px"
        );
        let show_more = send_card
            .query_selector("button.tool-show-more")
            .unwrap()
            .expect("the long message has an output disclosure");
        show_more.dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        assert_eq!(
            show_more.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        for appearance in [
            ChatAppearance::Current,
            ChatAppearance::Notebook,
            ChatAppearance::Ink,
        ] {
            state.chat_appearance.set(appearance);
            settle().await;
            let current_body = container
                .query_selector(".tool-send-message-body")
                .unwrap()
                .unwrap();
            assert!(
                body.is_same_node(Some(&current_body)),
                "appearance changes retain identical non-decision bodies"
            );
            let current_toggle = current_body
                .closest(".tool-card")
                .unwrap()
                .unwrap()
                .query_selector("button.tool-show-more")
                .unwrap()
                .unwrap();
            assert!(show_more.is_same_node(Some(&current_toggle)));
            assert_eq!(
                current_toggle.get_attribute("aria-expanded").as_deref(),
                Some("true"),
                "appearance changes retain the message output expansion"
            );
        }
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: None,
        }));
        let image = protocol::ImageData {
            media_type: "image/png".to_owned(),
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/lS8AAAAASUVORK5CYII=".to_owned(),
        };
        dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "flow-media".to_owned(),
            tool_name: "InspectArtifact".to_owned(),
            tool_type: protocol::ToolRequestType::RunCommand {
                command: "inspect /workspace/artifact.png".to_owned(),
                working_directory: "/workspace".to_owned(),
            },
        }));
        dispatch(ChatEvent::ToolExecutionCompleted(
            protocol::ToolExecutionCompletedData {
                tool_call_id: "flow-media".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::NativeContent {
                        text: "Artifact output remains selectable".to_owned(),
                        images: vec![image],
                    },
                },
            },
        ));
        let mut media_message = assistant("```rust\nlet artifact = true;\n```");
        media_message.tool_calls.push(protocol::ToolUseData {
            tool_call_id: "flow-media".to_owned(),
            name: "InspectArtifact".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: media_message,
        }));
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            state.tool_output_mode.set(mode);
            settle().await;
            let image = container
                .query_selector(".tool-native-content img")
                .unwrap();
            let media_header = visible(".tool-card-header")
                .into_iter()
                .find(|header| text_of(header).contains("InspectArtifact"))
                .unwrap();
            let media_card = media_header
                .parent_element()
                .unwrap()
                .dyn_into::<web_sys::HtmlDetailsElement>()
                .unwrap();
            assert_eq!(media_card.open(), mode == ToolOutputMode::Full);
            if mode == ToolOutputMode::Summary {
                assert!(
                    image.is_none(),
                    "Summary does not automatically mount raw native output"
                );
            }
            if !media_card.open() {
                media_header.dyn_ref::<HtmlElement>().unwrap().click();
                settle().await;
            }
            let native_request = media_card
                .query_selector(".tool-command-line")
                .unwrap()
                .expect("native command results retain their request in every expanded mode");
            assert_eq!(
                native_request.text_content().as_deref(),
                Some("inspect /workspace/artifact.png")
            );
            let image = media_card.query_selector("img").unwrap().unwrap();
            assert_eq!(image.get_attribute("alt").as_deref(), Some("Chat image"));
            let output = media_card
                .query_selector(".tool-raw-result")
                .unwrap()
                .unwrap();
            assert_eq!(
                output.text_content().as_deref(),
                Some("Artifact output remains selectable")
            );
            assert!(output.get_bounding_client_rect().height() > 0.0);
        }
        assert!(
            visible(".md-code-block pre")
                .iter()
                .any(|code| text_of(code).contains("let artifact = true;"))
        );
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: None,
        }));
        dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "flow-cancelled-command".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_type: protocol::ToolRequestType::RunCommand {
                command: commands[0].to_owned(),
                working_directory: "/workspace".to_owned(),
            },
        }));
        dispatch(ChatEvent::ToolExecutionCompleted(
            protocol::ToolExecutionCompletedData {
                tool_call_id: "flow-cancelled-command".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Cancelled {
                    message: "Command interrupted".to_owned(),
                },
            },
        ));
        let mut cancelled_message = assistant("");
        cancelled_message.tool_calls.push(protocol::ToolUseData {
            tool_call_id: "flow-cancelled-command".to_owned(),
            name: "Bash".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: cancelled_message,
        }));
        settle().await;
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            state.tool_output_mode.set(mode);
            settle().await;
            let cancelled_header = visible(".tool-card-header")
                .into_iter()
                .find(|header| text_of(header).contains("Cancelled"))
                .unwrap();
            let cancelled_card = cancelled_header
                .parent_element()
                .unwrap()
                .dyn_into::<web_sys::HtmlDetailsElement>()
                .unwrap();
            assert_eq!(cancelled_card.open(), mode == ToolOutputMode::Full);
            if !cancelled_card.open() {
                cancelled_header.dyn_ref::<HtmlElement>().unwrap().click();
                settle().await;
            }
            let literal = cancelled_card
                .query_selector(".tool-command-line")
                .unwrap()
                .expect("explicit cancellation details retain the request in Summary too");
            assert_eq!(literal.text_content().as_deref(), Some(commands[0]));
            assert!(
                literal.get_bounding_client_rect().height() > 0.0,
                "cancelled commands retain the full readable request in every mode"
            );
        }
        dispatch(ChatEvent::OperationCancelled(
            protocol::OperationCancelledData {
                message: "Operation cancelled".to_owned(),
            },
        ));
        settle().await;
        let notice = container
            .query_selector("[data-test=chat-notice-cancelled]")
            .unwrap()
            .unwrap();
        assert!(
            notice
                .query_selector(".chat-card-header")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect()
                .height()
                == 0.0,
            "one cancellation event has one quiet message, not a duplicate Cancelled heading"
        );
        let notice_body = notice.query_selector(".chat-card-body").unwrap().unwrap();
        let notice_style = web_sys::window()
            .unwrap()
            .get_computed_style(&notice_body)
            .unwrap()
            .unwrap();
        assert_eq!(
            notice_style.get_property_value("text-align").unwrap(),
            "left"
        );
        assert_eq!(
            notice_style.get_property_value("font-style").unwrap(),
            "normal"
        );
        assert_eq!(
            notice
                .query_selector(".chat-card-body")
                .unwrap()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("Operation cancelled")
        );
        container.style().set_property("width", "360px").unwrap();
        settle().await;
        let messages = container.query_selector(".chat-messages").unwrap().unwrap();
        assert!(
            messages.scroll_width() <= messages.client_width(),
            "narrow Flow contains expanded command, code, image, and failure output"
        );
        container.style().set_property("width", "800px").unwrap();
        settle().await;
        dispatch(ChatEvent::MessageAdded(protocol::ChatMessage {
            origin: None,
            message_id: None,
            timestamp: 10,
            sender: protocol::MessageSender::User,
            content: "Check reasoning-only stream and turn".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "codex".to_owned(),
            model: Some("gpt-test".to_owned()),
        }));
        dispatch(ChatEvent::StreamReasoningDelta(
            protocol::StreamTextDeltaData {
                text: "Planning before emitting any prose or tool.".to_owned(),
            },
        ));
        settle().await;
        assert!(
            container
                .query_selector(".chat-card-streaming")
                .unwrap()
                .is_none(),
            "reasoning-only stream in Flow with Show reasoning off leaves no empty card"
        );
        assert!(
            container
                .query_selector(".chat-card-thinking")
                .unwrap()
                .is_none(),
            "Flow never renders a replacement Thinking… line"
        );
        state.flow_show_reasoning.set(true);
        settle().await;
        assert!(
            container
                .query_selector(".chat-card-streaming .chat-card-reasoning")
                .unwrap()
                .is_some(),
            "enabling Show reasoning reveals the live reasoning disclosure in Flow"
        );
        assert!(
            container
                .query_selector(".chat-card-thinking")
                .unwrap()
                .is_none(),
            "Flow omits the redundant Thinking… line even when Show reasoning is on"
        );
        assert!(
            container
                .query_selector(".chat-card-streaming .chat-card-body")
                .unwrap()
                .is_none(),
            "Flow omits an empty body box while only reasoning is streaming"
        );
        let live_summary: HtmlElement = container
            .query_selector(".chat-card-streaming .chat-card-reasoning summary")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let live_sender: Element = container
            .query_selector(".chat-card-streaming .chat-card-sender")
            .unwrap()
            .unwrap();
        let font_px = |el: &Element| -> f64 {
            web_sys::window()
                .unwrap()
                .get_computed_style(el)
                .unwrap()
                .unwrap()
                .get_property_value("font-size")
                .unwrap()
                .trim_end_matches("px")
                .parse()
                .unwrap()
        };
        assert!(
            font_px(live_summary.as_ref()) < font_px(&live_sender),
            "live Flow reasoning summary is visually smaller than the sender header"
        );
        live_summary.click();
        settle().await;
        let live_reasoning = container
            .query_selector(".chat-card-streaming .reasoning-content")
            .unwrap()
            .expect("expanding Flow reasoning reveals the live trace");
        assert!(
            live_reasoning
                .text_content()
                .unwrap_or_default()
                .contains("Planning before emitting any prose or tool.")
        );
        state.flow_show_reasoning.set(false);
        settle().await;
        let assistant_cards_before = visible(".chat-card-assistant").len();
        let mut standalone_reasoning = assistant("");
        standalone_reasoning.reasoning = Some(protocol::ReasoningData {
            text: "Planning before emitting any prose or tool.".to_owned(),
            tokens: Some(9),
            signature: None,
            blob: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: standalone_reasoning,
        }));
        settle().await;
        assert_eq!(
            visible(".chat-card-assistant").len(),
            assistant_cards_before,
            "completed reasoning-only message leaves no empty card when Show reasoning is off"
        );
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "codex".to_owned(),
            model: Some("gpt-test".to_owned()),
        }));
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: "Visible answer after hidden reasoning.".to_owned(),
        }));
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: assistant("Visible answer after hidden reasoning."),
        }));
        settle().await;
        let reply_after_reasoning = visible(".chat-card-assistant")
            .into_iter()
            .find(|card| text_of(card).contains("Visible answer after hidden reasoning."))
            .expect("visible reply after hidden reasoning-only message");
        let reply_footer = reply_after_reasoning
            .query_selector(".chat-card-footer")
            .unwrap()
            .unwrap();
        assert!(
            reply_footer.get_bounding_client_rect().height() > 0.0
                && web_sys::window()
                    .unwrap()
                    .get_computed_style(&reply_footer)
                    .unwrap()
                    .unwrap()
                    .get_property_value("opacity")
                    .unwrap()
                    != "0",
            "byline footer on the visible reply is rendered and opaque"
        );
        state.flow_show_reasoning.set(true);
        settle().await;
        for summary in visible(".chat-card-reasoning summary") {
            summary.dyn_ref::<HtmlElement>().unwrap().click();
        }
        settle().await;
        assert!(
            visible(".reasoning-content")
                .iter()
                .any(|content| text_of(content)
                    .contains("Planning before emitting any prose or tool.")),
            "enabling Show reasoning and expanding reveals the completed reasoning-only message trace"
        );
        state.flow_show_reasoning.set(false);
        settle().await;
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn notebook_sets_visible_actions_beside_prose_and_flow_fills_the_pane() {
        use crate::appearance::ChatAppearance;
        use crate::state::ToolOutputMode;
        ensure_styles_loaded();
        let container = make_container();
        container.style().set_property("width", "960px").unwrap();
        container.style().set_property("height", "900px").unwrap();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-notebook".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-notebook".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <ChatView
                    tab_id=TabId(10_042)
                    agent_ref=Signal::derive(move || Some(bound_for_mount.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        state.chat_appearance.set(ChatAppearance::Notebook);
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        let assistant = |text: &str| {
            let mut message = mk_user_msg(text).message;
            message.sender = MessageSender::Assistant {
                agent: "claude".to_owned(),
            };
            message
        };
        let long_token = "unbroken".repeat(60);
        let long_path = format!("/workspace/{long_token}/probe.log");
        let tools = [
            ("notebook-ok", "Bash", "python3 first.py"),
            ("notebook-fail", "Bash", "python3 second.py"),
            (
                "notebook-mcp",
                "mcp__tyde-agent-control__tyde_create_workbench",
                "",
            ),
        ];
        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Inspect the workspace").message,
        ));
        for (index, (tool_call_id, tool_name, command)) in tools.into_iter().enumerate() {
            let text = match index {
                0 => "I'll inspect the workspace.".to_owned(),
                2 => format!("Both probes ran. {long_token}"),
                _ => String::new(),
            };
            dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
                agent: "claude".to_owned(),
                model: Some("claude-test".to_owned()),
            }));
            if !text.is_empty() {
                dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
                    text: text.clone(),
                }));
            }
            let tool_type = if command.is_empty() {
                protocol::ToolRequestType::Other {
                    args: serde_json::json!({ "name": "notebook" }),
                }
            } else {
                protocol::ToolRequestType::RunCommand {
                    command: command.to_owned(),
                    working_directory: "/workspace".to_owned(),
                }
            };
            dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                tool_type,
            }));
            let outcome = match index {
                0 => protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::RunCommand {
                        exit_code: 0,
                        stdout: "ok\n".to_owned(),
                        stderr: String::new(),
                    },
                },
                1 => protocol::ToolExecutionOutcome::Failed {
                    message: format!("Probe failed: cannot open {long_path}"),
                    details: Some(format!("{long_token}\n{long_path}")),
                    normalization_failure: None,
                    result: None,
                },
                _ => protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::Other {
                        result: serde_json::json!({ "created": true }),
                    },
                },
            };
            dispatch(ChatEvent::ToolExecutionCompleted(
                protocol::ToolExecutionCompletedData {
                    tool_call_id: tool_call_id.to_owned(),
                    outcome,
                },
            ));
            let mut message = assistant(&text);
            message.tool_calls.push(protocol::ToolUseData {
                tool_call_id: tool_call_id.to_owned(),
                name: tool_name.to_owned(),
                arguments: serde_json::json!({}),
                content_offset: None,
            });
            dispatch(ChatEvent::StreamEnd(protocol::StreamEndData { message }));
        }
        let error_texts = [
            format!("failed to start agent backend: {long_path}"),
            "No matching pending tool request for this response".to_owned(),
        ];
        for text in &error_texts {
            let mut error = mk_user_msg(text).message;
            error.sender = MessageSender::Error;
            dispatch(ChatEvent::MessageAdded(error));
        }

        async fn settle() {
            next_tick().await;
            next_animation_frame().await;
            next_animation_frame().await;
        }
        let window = web_sys::window().unwrap();
        let text_of = |element: &Element| element.text_content().unwrap_or_default();
        let visible = |selector: &str| {
            let nodes = container.query_selector_all(selector).unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
                .filter(|element| element.get_bounding_client_rect().height() > 0.0)
                .collect::<Vec<_>>()
        };
        let headers = || {
            let headers = visible(".tool-card-header");
            [
                "python3 first.py",
                "python3 second.py",
                "tyde_create_workbench",
            ]
            .iter()
            .map(|needle| {
                headers
                    .iter()
                    .find(|header| text_of(header).contains(needle))
                    .cloned()
                    .unwrap_or_else(|| panic!("action {needle} stays visible"))
            })
            .collect::<Vec<_>>()
        };
        let part = |header: &Element, selector: &str| {
            header
                .query_selector(selector)
                .unwrap()
                .filter(|element| element.get_bounding_client_rect().height() > 0.0)
        };
        let prose = || {
            visible(".chat-card-assistant .chat-card-body")
                .into_iter()
                .find(|body| text_of(body).contains("inspect the workspace"))
                .expect("assistant prose")
        };
        let user_text = || {
            visible(".chat-card-user .chat-card-body")
                .into_iter()
                .next()
                .expect("user prose")
        };
        let font_px = |element: &Element| {
            window
                .get_computed_style(element)
                .unwrap()
                .unwrap()
                .get_property_value("font-size")
                .unwrap()
                .trim_end_matches("px")
                .parse::<f64>()
                .unwrap()
        };
        let no_horizontal_overflow = |context: &str| {
            let messages = container.query_selector(".chat-messages").unwrap().unwrap();
            assert!(
                messages.scroll_width() <= messages.client_width(),
                "{context}: the transcript never scrolls sideways ({} > {})",
                messages.scroll_width(),
                messages.client_width()
            );
            let bounds = messages.get_bounding_client_rect();
            for header in headers() {
                let rect = header.get_bounding_client_rect();
                assert!(
                    rect.left() >= bounds.left() && rect.right() <= bounds.right() + 0.5,
                    "{context}: every action stays on screen"
                );
            }
        };
        let errors = || {
            let errors = visible(".chat-card-error");
            assert_eq!(errors.len(), 2, "every error row stays visible");
            for (error, text) in errors.iter().zip(&error_texts) {
                assert!(
                    text_of(error).contains(text.as_str()),
                    "the full technical message stays readable"
                );
            }
            errors
        };
        let quiet_errors = |style: &str| {
            let errors = errors();
            for error in &errors {
                let css = window.get_computed_style(error).unwrap().unwrap();
                assert_eq!(
                    css.get_property_value("background-color").unwrap(),
                    "rgba(0, 0, 0, 0)",
                    "{style} errors are not filled slabs"
                );
                assert_eq!(
                    css.get_property_value("border-top-width").unwrap(),
                    "0px",
                    "{style} errors are not boxed"
                );
                assert!(
                    error.scroll_width() <= error.client_width(),
                    "{style} wraps long unbroken error text inside its row"
                );
            }
            let gap = errors[1].get_bounding_client_rect().top()
                - errors[0].get_bounding_client_rect().bottom();
            assert!(
                (0.0..=12.0).contains(&gap),
                "{style} stacks consecutive errors closely: {gap}px"
            );
            errors
        };

        settle().await;
        let wide = headers();
        let labels: Vec<String> = wide
            .iter()
            .map(|header| {
                part(header, ".tool-card-action")
                    .map(|label| text_of(&label))
                    .expect("each action reads as words")
            })
            .collect();
        assert_eq!(
            labels,
            ["Ran command", "Run command", "Tyde create workbench"],
            "actions carry human labels in their real outcome"
        );
        for (header, name) in wide.iter().zip([
            "Bash",
            "Bash",
            "mcp__tyde-agent-control__tyde_create_workbench",
        ]) {
            assert_eq!(
                part(header, ".tool-card-name")
                    .map(|name| text_of(&name))
                    .as_deref(),
                Some(name),
                "the real tool identity stays visible beside its label"
            );
        }
        assert!(
            part(&wide[0], ".tool-card-detail")
                .is_some_and(|detail| text_of(&detail).contains("python3 first.py")),
            "the command itself stays visible"
        );
        assert!(text_of(&wide[1]).contains("Failed"));
        assert!(
            wide.windows(2)
                .all(|pair| pair[0].get_bounding_client_rect().top()
                    < pair[1].get_bounding_client_rect().top()),
            "actions stay chronological"
        );
        let body = prose().get_bounding_client_rect();
        let action = wide[0].get_bounding_client_rect();
        assert!(
            action.left() - body.right() >= 24.0,
            "wide panes separate actions from prose: prose ends {}, action starts {}",
            body.right(),
            action.left()
        );
        assert!(
            action.top() < body.bottom() && body.top() < action.bottom(),
            "an action sits beside the prose it belongs to"
        );
        assert!(
            action.width() >= 240.0 && body.width() >= 480.0,
            "neither column collapses: prose {}px, actions {}px",
            body.width(),
            action.width()
        );
        let user = user_text().get_bounding_client_rect();
        assert!(
            (user.left() - body.left()).abs() < 1.0 && user.right() <= action.left(),
            "the user's words read in the prose column, not a right bubble"
        );
        assert!(
            font_px(&prose()) > font_px(&part(&wide[0], ".tool-card-action").unwrap()),
            "prose reads larger than activity"
        );
        no_horizontal_overflow("wide");
        for error in quiet_errors("Notebook") {
            let rect = error.get_bounding_client_rect();
            assert!(
                rect.right() <= action.left() - 24.0 && rect.width() >= 400.0,
                "Notebook errors read in the prose column, clear of the actions: \
                 error {}..{}, actions start {}",
                rect.left(),
                rect.right(),
                action.left()
            );
        }

        let cards: Vec<web_sys::HtmlDetailsElement> = wide
            .iter()
            .map(|header| header.parent_element().unwrap().dyn_into().unwrap())
            .collect();
        assert!(cards.iter().all(|card| !card.open()));
        state.tool_output_mode.set(ToolOutputMode::Summary);
        settle().await;
        wide[0].dyn_ref::<HtmlElement>().unwrap().click();
        settle().await;
        let stdout = cards[0]
            .query_selector(".tool-result-stdout")
            .unwrap()
            .expect("an explicitly opened action shows its output");
        assert_eq!(stdout.text_content().unwrap().trim(), "ok");
        assert!(stdout.get_bounding_client_rect().height() > 0.0);
        assert!(!cards[1].open(), "Summary opens only the requested output");
        state.tool_output_mode.set(ToolOutputMode::Full);
        settle().await;
        assert!(
            cards.iter().all(|card| card.open()),
            "Full opens every output"
        );
        let failure = &cards[1];
        failure
            .query_selector(".tool-error-details summary")
            .unwrap()
            .expect("the failure keeps its details")
            .dyn_ref::<HtmlElement>()
            .unwrap()
            .click();
        settle().await;
        let column = failure.get_bounding_client_rect();
        let messages = container.query_selector(".chat-messages").unwrap().unwrap();
        let pane = messages.get_bounding_client_rect();
        assert!(
            column.left() >= pane.left() && column.right() <= pane.right() + 0.5,
            "the failed action stays inside the activity column"
        );
        for selector in [".tool-error-pre", ".tool-error-detail"] {
            let text = failure.query_selector(selector).unwrap().unwrap();
            let rect = text.get_bounding_client_rect();
            assert!(
                rect.height() > 0.0
                    && rect.left() >= column.left() - 0.5
                    && rect.right() <= column.right() + 0.5
                    && text.scroll_width() <= text.client_width(),
                "long unbroken failure text wraps inside its action: {selector} \
                 {}..{} in {}..{}",
                rect.left(),
                rect.right(),
                column.left(),
                column.right()
            );
            assert!(text_of(&text).contains(&long_path));
        }
        no_horizontal_overflow("expanded failure details");
        state.tool_output_mode.set(ToolOutputMode::Compact);
        settle().await;

        container.style().set_property("width", "600px").unwrap();
        settle().await;
        let narrow = headers();
        let body = prose().get_bounding_client_rect();
        let action = narrow[0].get_bounding_client_rect();
        assert!(
            action.top() >= body.bottom() && action.left() >= body.left(),
            "narrow panes stack actions under their prose"
        );
        assert!(
            body.width() >= 480.0,
            "narrow prose keeps the whole pane: {}px",
            body.width()
        );
        no_horizontal_overflow("narrow");
        for error in quiet_errors("Narrow Notebook") {
            let rect = error.get_bounding_client_rect();
            assert!(
                rect.left() <= body.left() + 1.0 && rect.right() >= body.right() - 1.0,
                "narrow Notebook errors span the prose column"
            );
        }
        container.style().set_property("width", "960px").unwrap();
        settle().await;

        let root = window
            .document()
            .unwrap()
            .document_element()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        let original_style = root.get_attribute("style");
        root.style()
            .set_property("--font-sans", "monospace")
            .unwrap();
        for size in [11.0, 20.0] {
            root.style()
                .set_property("--base-font-size", &format!("{size}px"))
                .unwrap();
            settle().await;
            let body = prose();
            let ratio = font_px(&body) / size;
            assert!(
                (ratio - 1.154).abs() < 0.01,
                "Notebook prose scales with the configured size: {ratio}"
            );
            assert!(
                window
                    .get_computed_style(&body)
                    .unwrap()
                    .unwrap()
                    .get_property_value("font-family")
                    .unwrap()
                    .contains("monospace"),
                "Notebook honors the selected typeface"
            );
            no_horizontal_overflow("custom font");
            quiet_errors("Notebook with a custom font");
        }
        match original_style {
            Some(style) => root.set_attribute("style", &style).unwrap(),
            None => root.remove_attribute("style").unwrap(),
        }

        let no_notebook_labels = |style: &str| {
            for header in headers() {
                let text = text_of(&header);
                for label in ["Ran command", "Run command", "Tyde create workbench"] {
                    assert!(
                        !text.contains(label),
                        "{style} keeps its existing action rows without \"{label}\""
                    );
                }
            }
        };
        state.chat_appearance.set(ChatAppearance::Ink);
        settle().await;
        for header in headers() {
            let action = header.query_selector(".tool-card-action").unwrap().unwrap();
            let command = header
                .closest("[data-tool-kind=command]")
                .unwrap()
                .is_some();
            assert_eq!(
                action.get_bounding_client_rect().height() > 0.0,
                !command,
                "Flow shows human-readable actions, but commands retain their actual tool name"
            );
        }
        let messages = container.query_selector(".chat-messages").unwrap().unwrap();
        let bounds = messages.get_bounding_client_rect();
        let style = window.get_computed_style(&messages).unwrap().unwrap();
        let inset = |side: &str| {
            style
                .get_property_value(&format!("padding-{side}"))
                .unwrap()
                .trim_end_matches("px")
                .parse::<f64>()
                .unwrap()
        };
        let pane_right = bounds.left() + f64::from(messages.client_width()) - inset("right");
        let body = prose().get_bounding_client_rect();
        assert!(
            (body.left() - (bounds.left() + inset("left"))).abs() < 1.0
                && (pane_right - body.right()).abs() < 1.0,
            "Flow prose fills the chat pane between its gutters: {}..{} of {}..{}",
            body.left(),
            body.right(),
            bounds.left() + inset("left"),
            pane_right
        );
        no_horizontal_overflow("Flow");
        for error in quiet_errors("Flow") {
            assert!(
                error.get_bounding_client_rect().right() <= pane_right + 1.0,
                "Flow errors stay inside the pane"
            );
        }

        state.chat_appearance.set(ChatAppearance::Current);
        settle().await;
        no_notebook_labels("Default");
        for error in errors() {
            let css = window.get_computed_style(&error).unwrap().unwrap();
            assert_ne!(
                css.get_property_value("background-color").unwrap(),
                "rgba(0, 0, 0, 0)",
                "Default keeps its existing filled error card"
            );
            assert_eq!(
                css.get_property_value("border-top-width").unwrap(),
                "1px",
                "Default keeps its existing error frame"
            );
        }
        assert_eq!(visible(".tool-card-header").len(), 3);
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn flow_keeps_every_action_visible_in_long_lists() {
        use crate::appearance::ChatAppearance;
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-ink-overflow".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-ink-overflow".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <ChatView
                    tab_id=TabId(10_043)
                    agent_ref=Signal::derive(move || Some(bound_for_mount.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        state.chat_appearance.set(ChatAppearance::Ink);
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        async fn settle() {
            next_tick().await;
            next_animation_frame().await;
            next_animation_frame().await;
        }
        let visible = |selector: &str| {
            let nodes = container.query_selector_all(selector).unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
                .filter(|element| element.get_bounding_client_rect().height() > 0.0)
                .collect::<Vec<_>>()
        };

        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Run the batch").message,
        ));
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "claude".to_owned(),
            model: Some("claude-test".to_owned()),
        }));
        let mut message = mk_user_msg("Running the batch.").message;
        message.sender = MessageSender::Assistant {
            agent: "claude".to_owned(),
        };
        for index in 0..90 {
            let tool_call_id = format!("ink-overflow-{index}");
            let command = format!("python3 step_{index}.py");
            dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
                tool_call_id: tool_call_id.clone(),
                tool_name: "Bash".to_owned(),
                tool_type: protocol::ToolRequestType::RunCommand {
                    command: command.clone(),
                    working_directory: "/workspace".to_owned(),
                },
            }));
            dispatch(ChatEvent::ToolExecutionCompleted(
                protocol::ToolExecutionCompletedData {
                    tool_call_id: tool_call_id.clone(),
                    outcome: protocol::ToolExecutionOutcome::Succeeded {
                        result: protocol::ToolExecutionResult::RunCommand {
                            exit_code: 0,
                            stdout: "ok\n".to_owned(),
                            stderr: String::new(),
                        },
                    },
                },
            ));
            message.tool_calls.push(protocol::ToolUseData {
                tool_call_id,
                name: "Bash".to_owned(),
                arguments: serde_json::json!({ "command": command }),
                content_offset: None,
            });
        }
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData { message }));
        settle().await;

        assert!(
            visible(".tool-list-summary").is_empty(),
            "Flow never folds completed actions into an aggregate"
        );
        let headers = visible(".tool-card-header");
        assert_eq!(headers.len(), 90);
        for (index, header) in headers.iter().enumerate() {
            assert!(
                header
                    .text_content()
                    .unwrap_or_default()
                    .contains(&format!("python3 step_{index}.py")),
                "long lists retain chronological action labels"
            );
        }
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn steering_preserves_the_position_of_a_running_tool_response() {
        ensure_styles_loaded();
        let container = make_container();
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None::<AppState>));
        let setup_handle = state_handle.clone();
        let agent_id = AgentId("agent-steer-order".to_owned());
        let bound = ActiveAgentRef {
            host_id: "host-steer-order".to_owned(),
            agent_id: agent_id.clone(),
        };
        let bound_for_mount = bound.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <ChatView
                    tab_id=TabId(10_005)
                    agent_ref=Signal::derive(move || Some(bound_for_mount.clone()))
                    is_active=Signal::derive(|| true)
                />
            }
        });
        next_tick().await;
        let state = state_handle.borrow().clone().expect("mounted state");
        let dispatch = |event| {
            crate::dispatch::apply_chat_event(&state, &bound.host_id, &agent_id, event);
        };
        dispatch(ChatEvent::TypingStatusChanged(true));
        dispatch(ChatEvent::StreamStart(protocol::StreamStartData {
            agent: "codex".to_owned(),
            model: None,
        }));
        dispatch(ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
            text: "The original task is still running".to_owned(),
        }));
        dispatch(ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "steer-order-sleep".to_owned(),
            tool_name: "sleep".to_owned(),
            tool_type: protocol::ToolRequestType::Sleep {
                duration_ms: 230000,
            },
        }));
        next_tick().await;
        next_animation_frame().await;
        assert!(container.text_content().unwrap().contains("sleep"));

        dispatch(ChatEvent::MessageAdded(
            mk_user_msg("Summarize the task for handoff").message,
        ));
        next_tick().await;
        next_animation_frame().await;
        let measure_order = |phase: &str| {
            let assistant = query(&container, ".chat-card-assistant")
                .expect("the original response remains visible");
            let user =
                query(&container, ".chat-card-user").expect("the steering message is visible");
            assert!(assistant.text_content().unwrap().contains("sleep"));
            assert!(
                user.text_content()
                    .unwrap()
                    .contains("Summarize the task for handoff")
            );
            let gap = user.get_bounding_client_rect().top()
                - assistant.get_bounding_client_rect().bottom();
            console_log!("Steering transcript order: phase={phase}, gap={gap}px");
            gap
        };
        let running_gap = measure_order("running");

        dispatch(ChatEvent::ToolExecutionCompleted(
            protocol::ToolExecutionCompletedData {
                tool_call_id: "steer-order-sleep".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::Sleep,
                },
            },
        ));
        let mut completed = mk_user_msg("The original task is still running").message;
        completed.sender = MessageSender::Assistant {
            agent: "codex".to_owned(),
        };
        completed.tool_calls.push(protocol::ToolUseData {
            tool_call_id: "steer-order-sleep".to_owned(),
            name: "sleep".to_owned(),
            arguments: serde_json::json!({"duration_ms": 230000}),
            content_offset: None,
        });
        dispatch(ChatEvent::StreamEnd(protocol::StreamEndData {
            message: completed,
        }));
        next_tick().await;
        next_animation_frame().await;
        let completed_gap = measure_order("completed");
        assert!(container.text_content().unwrap().contains("Done"));
        drop(handle);
        container.remove();
        assert!(
            running_gap >= 0.0 && completed_gap >= 0.0,
            "a steer must stay below the response already executing its tool, both while running and after completion: running gap={running_gap}px, completed gap={completed_gap}px"
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

    #[wasm_bindgen_test]
    async fn windowed_list_does_not_mount_all_rows_for_long_transcript() {
        use crate::components::center_zone::CenterZone;
        use crate::perf::diagnostic_tests::Capture;
        use crate::state::TabContent;

        ensure_styles_loaded();
        let capture = Capture::browser();
        capture.start();
        capture.stop();
        let disabled = capture.export();
        let total_rows = 200usize;
        let agent_id = AgentId("agent-virt-private".to_owned());
        let host_id = "host-virt-private".to_owned();
        let state = AppState::new();
        state
            .chat_appearance
            .set(crate::appearance::ChatAppearance::Current);
        state
            .agents
            .set(vec![make_target_agent(&host_id, &agent_id.0, None)]);
        state.chat_rows.update(|m| {
            m.insert(
                agent_id.clone(),
                (0..total_rows)
                    .map(|i| {
                        ChatRowHandle::new(mk_user_msg(&format!(
                            "private diagnostic fixture {i} {}",
                            "generated payload ".repeat(64)
                        )))
                    })
                    .collect(),
            );
        });
        let tab_id = state
            .open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id,
                    agent_id: agent_id.clone(),
                }),
                "Long chat".to_owned(),
                true,
            )
            .unwrap();
        state.bump_tab_lru(tab_id);
        state.save_tab_scroll_state(
            tab_id,
            TabScrollState {
                scroll_top: 0,
                scroll_height: 40_000,
                client_height: 600,
                user_scrolled_up: true,
            },
        );
        let container = make_container();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            view! { <CenterZone /> }
        });
        next_tick().await;
        next_animation_frame().await;
        next_tick().await;
        assert_eq!(
            capture.export(),
            disabled,
            "projections must not start a capture"
        );

        let mounted = message_rows(&container);
        assert!(!mounted.is_empty(), "the transcript must render");
        assert!(
            mounted.len() < total_rows,
            "windowing must leave offscreen rows unmounted"
        );
        let first = mounted[0].clone();
        let spacer: HtmlElement = container
            .query_selector(".virt-spacer-bottom")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            spacer.get_bounding_client_rect().height() > 0.0,
            "the spacer must reserve geometry for unmounted rows"
        );

        capture.start();
        let other = state
            .open_tab(TabContent::empty_chat(), "Other".to_owned(), true)
            .unwrap();
        state.bump_tab_lru(other);
        state.chat_rows.update(|rows| {
            rows.insert(AgentId("unrelated-private".into()), Vec::new());
        });
        next_tick().await;
        next_tick().await;
        let current = capture.export();
        let records = current["records"].as_array().unwrap();
        for operation in [
            "chat_row_projection",
            "chat_context_scan",
            "chat_context_reported_scan",
            "chat_virtual_window",
        ] {
            assert!(
                records
                    .iter()
                    .any(|r| r["operation"] == operation && r["items"] == total_rows),
                "missing long-transcript attribution for {operation}"
            );
        }
        assert!(records.iter().any(|r| r["operation"] == "chat_tab_binding"));
        assert!(
            !records.iter().any(|r| r["operation"] == "chat_ink_layout"),
            "Default appearance must not claim Ink layout work"
        );
        assert!(
            first.is_connected(),
            "the hidden hot tab must remain mounted"
        );
        let hidden = first.closest("[role='tabpanel']").unwrap().unwrap();
        assert_eq!(
            web_sys::window()
                .unwrap()
                .get_computed_style(&hidden)
                .unwrap()
                .unwrap()
                .get_property_value("display")
                .unwrap(),
            "none"
        );
        let encoded = current.to_string();
        for private in [
            "private diagnostic fixture",
            "generated payload",
            "agent-virt-private",
            "host-virt-private",
            "unrelated-private",
        ] {
            assert!(
                !encoded.contains(private),
                "capture must not retain fixture content or identifiers"
            );
        }
        assert_eq!(current["loss"]["recordsOverwritten"], 0);
        assert_eq!(current["loss"]["activeDropped"], 0);
        assert!(current["active"].as_array().unwrap().is_empty());

        capture.start();
        state
            .chat_appearance
            .set(crate::appearance::ChatAppearance::Ink);
        next_tick().await;
        next_tick().await;
        let ink = capture.export();
        let ink_records = ink["records"].as_array().unwrap();
        let layout = ink_records
            .iter()
            .find(|r| r["operation"] == "chat_ink_layout" && r["items"] == total_rows)
            .expect("Ink must attribute the loaded history layout");
        assert!(
            ink_records
                .iter()
                .any(|r| r["operation"] == "chat_row_projection" && r["parent"] == layout["id"]),
            "nested projection must preserve capture-local parent correlation"
        );

        capture.start();
        state
            .agents
            .update(|agents| agents[0].backend_kind = BackendKind::Codex);
        state.chat_rows.update(|rows| {
            rows.remove(&AgentId("unrelated-private".into()));
        });
        next_tick().await;
        next_tick().await;
        capture.stop();
        let codex = capture.export();
        let records = codex["records"].as_array().unwrap();
        assert!(
            records
                .iter()
                .any(|r| r["operation"] == "chat_virtual_window" && r["items"] == total_rows)
        );
        assert!(
            !records
                .iter()
                .any(|r| r["operation"] == "chat_context_scan"),
            "Codex must continue to bypass the transcript context-breakdown scan"
        );
        // The pinned browser runner streams result output, not successful-test console logs.
        let output = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("output")
            .expect("canonical browser result output is present");
        let mut result_text = output.text_content().unwrap();
        for (appearance, observation) in [
            ("default", &current),
            ("flow", &ink),
            ("codex-flow", &codex),
        ] {
            result_text.push_str(&format!(
                "UI_PROJECTION_CAPTURE {appearance} {observation}\n"
            ));
        }
        output.set_text_content(Some(&result_text));
        state.rename_tab_label(other, "Renamed".into());
        next_tick().await;
        next_tick().await;
        assert_eq!(
            capture.export(),
            codex,
            "stopped captures must remain immutable"
        );
        drop(handle);
        container.remove();
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
                    tyde_permissions: Default::default(),
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
                    review_recipient: false,
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
            tyde_permissions: Default::default(),
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
            review_recipient: false,
        }
    }

    #[wasm_bindgen_test]
    async fn tyde_permissions_menu_is_icon_only_server_driven_and_targets_its_pane() {
        ensure_styles_loaded();
        let trace = |phase: &str| {
            let output = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id("output")
                .unwrap();
            output.set_text_content(Some(&format!(
                "{}Tyde role DOM phase: {phase}\n",
                output.text_content().unwrap()
            )));
        };
        struct BridgeFixture;
        impl Drop for BridgeFixture {
            fn drop(&mut self) {
                js_sys::eval("window.__TAURI__ = window.__rolePrevious; delete window.__rolePrevious; delete window.__roleFrames;").unwrap();
            }
        }
        js_sys::eval(r#"window.__rolePrevious = window.__TAURI__; window.__roleFrames = [];
            window.__TAURI__ = { core: { invoke: (cmd,args) => {
                if(cmd === 'send_host_line') { const frame = JSON.parse(args.line); if(frame.kind === 'set_agent_tyde_permissions') window.__roleFrames.push({host:args.hostId,frame}); }
                return Promise.resolve();
            } } };"#).unwrap();
        let _bridge = BridgeFixture;
        let selected = RwSignal::new(Some(ActiveAgentRef {
            host_id: "role-host-a".to_owned(),
            agent_id: AgentId("same-agent".to_owned()),
        }));
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let mounted = slot.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            for (host, access) in [
                ("role-host-a", protocol::AgentTydePermissions::Root),
                ("role-host-b", protocol::AgentTydePermissions::Agent),
            ] {
                crate::dispatch::prime_host_with_bootstrap_for_tests(&state, host, |bootstrap| {
                    bootstrap.agents.push(protocol::NewAgentPayload {
                        swarm_membership: None,
                        agent_id: AgentId("same-agent".to_owned()),
                        name: "same-agent".to_owned(),
                        origin: AgentOrigin::User,
                        tyde_permissions: access,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        workspace_roots: Vec::new(),
                        custom_agent_id: None,
                        team_id: None,
                        team_member_id: None,
                        project_id: None,
                        parent_agent_id: None,
                        session_id: Some(protocol::SessionId("saved-role-session".to_owned())),
                        workflow: None,
                        created_at_ms: 0,
                        instance_stream: StreamPath("/agent/same-agent".to_owned()),
                        activity_summary: Default::default(),
                        activity: protocol::AgentActivity::Idle,
                        review_recipient: false,
                    });
                });
            }
            mounted.borrow_mut().replace(state.clone());
            provide_context(state);
            let target = Memo::new(move |_| selected.get());
            view! { <AgentTydePermissionsMenu agent_ref=target.into() /> }
        });
        next_tick().await;
        let state = slot.borrow_mut().take().unwrap();
        let button: HtmlElement = container
            .query_selector("button[aria-label='Tyde permissions']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        trace(&format!(
            "mounted; title={:?}; disabled={:?}",
            button.get_attribute("title"),
            button.get_attribute("disabled")
        ));
        assert!(button.text_content().unwrap().trim().is_empty());
        let style = web_sys::window()
            .unwrap()
            .get_computed_style(&button)
            .unwrap()
            .unwrap();
        trace(&format!(
            "style; background={}; border={}",
            style.get_property_value("background-color").unwrap(),
            style.get_property_value("border-top-width").unwrap()
        ));
        assert_eq!(
            style.get_property_value("background-color").unwrap(),
            "rgba(0, 0, 0, 0)"
        );
        assert_eq!(style.get_property_value("border-top-width").unwrap(), "0px");
        let neutral = style.get_property_value("color").unwrap();
        let choice = |label: &str| -> HtmlElement {
            let choices = container
                .query_selector_all("[role='menuitemradio']")
                .unwrap();
            (0..choices.length())
                .map(|i| choices.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
                .find(|item| item.text_content().unwrap().starts_with(label))
                .unwrap()
        };
        let frame = |index: usize| -> serde_json::Value {
            serde_wasm_bindgen::from_value(
                js_sys::eval(&format!("window.__roleFrames[{index}]")).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            button.get_attribute("title").unwrap(),
            "Tyde permissions: Root",
            "Saved host descriptors render their non-default role before live updates"
        );
        button.click();
        next_tick().await;
        assert_eq!(
            choice("Root").get_attribute("aria-checked").unwrap(),
            "true"
        );
        let reload = |access| {
            crate::dispatch::prime_agent_stream_for_tests(
                &state,
                "role-host-a",
                &StreamPath("/agent/same-agent".to_owned()),
                &protocol::AgentStartPayload {
                    swarm_membership: None,
                    agent_id: AgentId("same-agent".to_owned()),
                    name: "same-agent".to_owned(),
                    origin: AgentOrigin::User,
                    tyde_permissions: access,
                    backend_kind: BackendKind::Claude,
                    launch_profile_id: None,
                    workspace_roots: Vec::new(),
                    custom_agent_id: None,
                    team_id: None,
                    team_member_id: None,
                    project_id: None,
                    parent_agent_id: None,
                    session_id: Some(protocol::SessionId("saved-role-session".to_owned())),
                    workflow: None,
                    created_at_ms: 0,
                },
            );
        };
        reload(protocol::AgentTydePermissions::Orchestrator);
        next_tick().await;
        trace(&format!(
            "Orchestrator bootstrap; title={:?}; open={:?}",
            button.get_attribute("title"),
            button.get_attribute("aria-expanded")
        ));
        assert_eq!(
            button.get_attribute("title").unwrap(),
            "Tyde permissions: Tyde Orchestrator"
        );
        assert_eq!(
            choice("Tyde Orchestrator")
                .get_attribute("aria-checked")
                .unwrap(),
            "true",
            "Attached reload/resume bootstrap overrides the earlier host descriptor"
        );
        reload(protocol::AgentTydePermissions::Agent);
        next_tick().await;
        trace("Agent bootstrap complete");
        assert_eq!(
            choice("Agent").get_attribute("aria-checked").unwrap(),
            "true"
        );
        button.click();
        next_tick().await;
        let apply = |host: &str, seq, access| {
            crate::dispatch::dispatch_envelope(
                &state,
                host,
                protocol::Envelope::from_payload(
                    StreamPath(format!("/host/{host}")),
                    protocol::FrameKind::AgentTydePermissionsChanged,
                    seq,
                    &protocol::AgentTydePermissionsChangedPayload {
                        agent_id: AgentId("same-agent".to_owned()),
                        access,
                    },
                )
                .unwrap(),
            )
        };
        button.click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[role='menuitemradio']")
                .unwrap()
                .length(),
            3
        );
        for text in [
            "Tyde permissions",
            "Control own sub-agents",
            "Add new steering and review aspects",
            "Control any agent + anything in Tyde",
        ] {
            assert!(container.text_content().unwrap().contains(text));
        }
        choice("Tyde Orchestrator").click();
        next_tick().await;
        trace(&format!(
            "first selection; sent count={:?}",
            js_sys::eval("window.__roleFrames.length").unwrap().as_f64()
        ));
        assert_eq!(frame(0)["host"], "role-host-a");
        assert_eq!(frame(0)["frame"]["stream"], "/agent/same-agent");
        assert_eq!(frame(0)["frame"]["payload"]["access"], "orchestrator");
        button.click();
        next_tick().await;
        assert_eq!(
            choice("Agent").get_attribute("aria-checked").unwrap(),
            "true",
            "No optimistic authority change"
        );
        apply(
            "role-host-a",
            0,
            protocol::AgentTydePermissions::Orchestrator,
        );
        next_tick().await;
        assert_eq!(
            choice("Tyde Orchestrator")
                .get_attribute("aria-checked")
                .unwrap(),
            "true"
        );
        assert_eq!(
            web_sys::window()
                .unwrap()
                .get_computed_style(&button)
                .unwrap()
                .unwrap()
                .get_property_value("color")
                .unwrap(),
            neutral
        );
        trace("live Orchestrator verified; switching hosts");
        selected.set(Some(ActiveAgentRef {
            host_id: "role-host-b".to_owned(),
            agent_id: AgentId("same-agent".to_owned()),
        }));
        next_tick().await;
        trace(&format!(
            "switched pane; title={:?}; open={:?}",
            button.get_attribute("title"),
            button.get_attribute("aria-expanded")
        ));
        assert_eq!(button.get_attribute("aria-expanded").unwrap(), "false");
        assert_eq!(
            button.get_attribute("title").unwrap(),
            "Tyde permissions: Agent"
        );
        button.click();
        next_tick().await;
        choice("Root").click();
        next_tick().await;
        trace("second host selection");
        assert_eq!(frame(1)["host"], "role-host-b");
        assert_eq!(frame(1)["frame"]["payload"]["access"], "root");
        apply("role-host-b", 0, protocol::AgentTydePermissions::Root);
        next_tick().await;
        assert_eq!(
            button.get_attribute("title").unwrap(),
            "Tyde permissions: Root"
        );
        selected.set(Some(ActiveAgentRef {
            host_id: "role-host-a".to_owned(),
            agent_id: AgentId("same-agent".to_owned()),
        }));
        next_tick().await;
        assert_eq!(
            button.get_attribute("title").unwrap(),
            "Tyde permissions: Tyde Orchestrator"
        );
        button.click();
        next_tick().await;
        trace("demotion selection");
        choice("Agent").click();
        next_tick().await;
        assert_eq!(frame(2)["frame"]["payload"]["access"], "agent");
        apply("role-host-a", 1, protocol::AgentTydePermissions::Agent);
        next_tick().await;
        button.click();
        next_tick().await;
        let init = web_sys::KeyboardEventInit::new();
        trace("escape dismissal");
        init.set_key("Escape");
        init.set_bubbles(true);
        button
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
        next_tick().await;
        assert_eq!(button.get_attribute("aria-expanded").unwrap(), "false");
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
            origin: None,
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
            status: protocol::AgentControlStatus::Idle,
            error: None,
            events: vec![
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(tycode_terminal_message())),
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(false)),
            ],
            latest_output: Default::default(),
            activity: protocol::AgentActivity::Idle,
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
            status: protocol::AgentControlStatus::Idle,
            error: None,
            events,
            latest_output: Default::default(),
            activity: protocol::AgentActivity::Idle,
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
                id: None,
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

        let assert_order = |notice: &Element| {
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
        };
        assert_order(&notice);
        state
            .chat_appearance
            .set(crate::appearance::ChatAppearance::Ink);
        next_tick().await;
        let ink_notice = query(&container, "[data-test='chat-notice-retry']").unwrap();
        let details = ink_notice
            .clone()
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .unwrap();
        let summary = ink_notice.query_selector("summary").unwrap().unwrap();
        let error = ink_notice.query_selector("pre").unwrap().unwrap();
        assert!(!details.open());
        assert!(
            !summary
                .text_content()
                .unwrap_or_default()
                .contains("Rate Limited"),
            "generic typed retries must not claim a rate-limit classification"
        );
        assert!(
            summary
                .text_content()
                .unwrap_or_default()
                .contains("Retry 1/3")
        );
        // Chrome reports a nonzero descendant rect inside closed details.
        // Test painted text and the enclosing row's space, not that rect.
        wasm_bindgen_test::console_log!(
            "Closed retry layout: descendant_height={} row_height={} summary_height={}",
            error.get_bounding_client_rect().height(),
            ink_notice.get_bounding_client_rect().height(),
            summary.get_bounding_client_rect().height()
        );
        assert!(
            !details.inner_text().contains("429 Too Many Requests")
                && !details.inner_text().contains("Reported backoff"),
            "closed retry details do not expose their text"
        );
        assert!(
            (ink_notice.get_bounding_client_rect().height()
                - summary.get_bounding_client_rect().height())
            .abs()
                < 0.5,
            "the retry record is one restrained summary row"
        );
        summary.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;
        assert!(details.open());
        assert!(error.get_bounding_client_rect().height() > 0.0);
        assert!(
            error
                .text_content()
                .unwrap_or_default()
                .contains("429 Too Many Requests")
        );
        assert!(
            details.inner_text().contains("Reported backoff: 4000ms"),
            "the exact protocol backoff remains accessible with the error details"
        );
        assert_order(&ink_notice);
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

    /// Compaction is not a chat-header action. The circular-arrow control
    /// sat next to the tool-output verbosity toggle and was read as the
    /// same control. Compact stays on the agent card, the command palette,
    /// and the team controls.
    #[wasm_bindgen_test]
    async fn chat_header_does_not_show_a_compact_control() {
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
            provide_context(state);
            let agent_ref = Signal::derive(move || Some(bound.clone()));
            let is_active: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView tab_id=TabId(20_004) agent_ref=agent_ref is_active=is_active /> }
        })
        .forget();
        next_tick().await;

        let header =
            query(&container, ".chat-agent-header").expect("the chat header still renders");
        assert!(
            header
                .query_selector("[data-test='chat-header-compact']")
                .unwrap()
                .is_none(),
            "compact is not a header action"
        );
        assert!(
            query(&container, "[data-test='chat-header-compact']").is_none(),
            "no leftover compact control is mounted outside the header either"
        );
    }
}
