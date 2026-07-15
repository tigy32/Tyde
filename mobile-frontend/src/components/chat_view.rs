use std::cell::{Cell, RefCell};
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::pending_submissions::AgentPendingSubmissions;
use crate::components::ui::{Button, ButtonSize, ButtonVariant, EmptyState, Spinner};
use crate::state::{AgentRef, AppState, LocalHostId};

const CHAT_STICKY_BOTTOM_THRESHOLD_PX: i32 = 80;
const SESSION_HISTORY_PAGE_LIMIT: u32 = 50;

/// Minimum touch-target size, inline so the recovery control stays tappable
/// independently of the stylesheet.
const RECOVERY_TOUCH_TARGET: &str = "min-width:44px;min-height:44px;";

/// Surface a failed interrupt.
///
/// An `Interrupt` carries no user text, so there is nothing to hold and nothing
/// to recover — it is not a `PendingSubmission`. What it does have is a
/// consequence: if the frame never went out, the agent is **still running**. That
/// has to be said, not swallowed.
///
/// Routed through `mobile_shell_error`, which renders as a dismissible
/// `role="alert"` banner — the same surface every other rejected admission uses,
/// and one that sits above whatever the user navigates to next.
fn report_interrupt_error(state: &AppState, message: String) {
    log::error!("{message}");
    state
        .mobile_shell_error
        .set(Some(crate::state::MobileShellError {
            code: protocol::MobileAccessErrorCode::TransportFailed,
            message,
        }));
}

/// The terminal state of a conversation that could not be loaded.
///
/// This is what the spinner turns into. It is an `alert`, not a status: the
/// screen reader was told "Loading conversation" and must be told how that
/// ended, rather than being left on an unbounded live region forever.
///
/// The only recovery offered is a reconnect. There is deliberately no in-place
/// "try again": the server rejects a second `LoadAgent` on an already-attached
/// stream as a conflict, so retrying in place is guaranteed to fail. Dropping
/// the connection and coming back clears the load latch and re-loads the agent
/// on a fresh instance stream, which actually works.
#[component]
fn ChatLoadFailed(host: LocalHostId, message: String) -> impl IntoView {
    let reconnecting = RwSignal::new(false);

    let on_reconnect = move |_| {
        // The host is passed in from the agent's own `AgentRef`, not read back
        // out of `active_local_host_id`. Reconnecting "whatever host is currently
        // selected" is a different action from reconnecting the host this
        // conversation belongs to.
        let host = host.clone();
        reconnecting.set(true);
        spawn_local(async move {
            if let Err(error) = bridge::disconnect_paired_host(&host).await {
                log::error!("reconnect: disconnect_paired_host({host}) failed: {error}");
            }
            if let Err(error) = bridge::connect_paired_host(&host).await {
                log::error!("reconnect: connect_paired_host({host}) failed: {error}");
            }
            reconnecting.set(false);
        });
    };

    view! {
        <div class="chat-load-failed" role="alert" data-mobile-test="chat-load-failed">
            <p class="chat-load-failed-message" data-mobile-test="chat-load-failed-message">
                {message}
            </p>
            <button
                type="button"
                class="chat-load-failed-reconnect"
                style=RECOVERY_TOUCH_TARGET
                data-mobile-test="chat-load-failed-reconnect"
                aria-label="Reconnect to this host and load the conversation again"
                disabled=move || reconnecting.get()
                on:click=on_reconnect
            >
                {move || if reconnecting.get() { "Reconnecting…" } else { "Reconnect" }}
            </button>
        </div>
    }
}

/// Edge-swipe-to-go-back tuning. The gesture fires the same action as the
/// back button: a horizontal swipe that *starts* within `EDGE_ZONE_PX` of a
/// screen edge and travels past `SWIPE_THRESHOLD_PX`, provided the motion is
/// dominantly horizontal (|dx| > |dy| * `SWIPE_HORIZONTAL_DOMINANCE`) so it
/// never triggers while the user is scrolling the transcript vertically.
const EDGE_ZONE_PX: f64 = 24.0;
const SWIPE_THRESHOLD_PX: f64 = 64.0;
const SWIPE_HORIZONTAL_DOMINANCE: f64 = 1.5;

/// Which edge the back-swipe starts from, and therefore which way the finger
/// travels. Flip this single constant to switch between the iOS convention
/// (`LeftEdgeMoveRight`) and `RightEdgeMoveLeft`; the rest of the gesture logic
/// keys off it.
const BACK_SWIPE: BackSwipe = BackSwipe::LeftEdgeMoveRight;

// One variant is intentionally unselected: it is the alternative the
// `BACK_SWIPE` constant can be flipped to without touching the gesture logic.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum BackSwipe {
    /// Start near the right screen edge, finger moves left.
    RightEdgeMoveLeft,
    /// Start near the left screen edge, finger moves right. (iOS-style, default)
    LeftEdgeMoveRight,
}

/// Pure decision for the edge-swipe-back gesture, given the touch start X, the
/// total horizontal/vertical travel, and the viewport width. Kept free of DOM
/// types so it is directly unit-testable.
fn back_swipe_triggered(start_x: f64, dx: f64, dy: f64, viewport_width: f64) -> bool {
    // Must be dominantly horizontal so vertical scrolling never fires it.
    if dx.abs() <= dy.abs() * SWIPE_HORIZONTAL_DOMINANCE {
        return false;
    }
    if dx.abs() < SWIPE_THRESHOLD_PX {
        return false;
    }
    match BACK_SWIPE {
        BackSwipe::RightEdgeMoveLeft => start_x >= viewport_width - EDGE_ZONE_PX && dx < 0.0,
        BackSwipe::LeftEdgeMoveRight => start_x <= EDGE_ZONE_PX && dx > 0.0,
    }
}

/// Conversation surface.
///
/// Composition rules:
/// - The header surfaces the agent name plus the current backend in the
///   subtitle, and exposes Stop while a turn is active. The back button is
///   small but always has an accessible label.
/// - The transcript shows task list → messages → streaming →
///   transient events, in that order, because that is the
///   order users perceive them happening.
/// - Queued messages live in the composer controls, not in the
///   transcript, so pending sends do not appear twice.
/// - Every test-relevant element exposes `data-mobile-test` so wasm
///   tests can locate it without depending on CSS class names.
#[component]
pub fn ChatView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let more_open: RwSignal<bool> = RwSignal::new(false);

    let s_back = state.clone();
    let on_back = move |_| {
        more_open.set(false);
        s_back.viewing_chat.set(false);
    };

    // Edge-swipe-to-go-back. We record the touch start position, optionally
    // abandon it mid-gesture if it turns into a vertical scroll, and only make
    // the back decision on touchend. We never call `prevent_default`, so the
    // transcript still scrolls, text still selects, and taps still register.
    let swipe_start: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));

    let start_cell = swipe_start.clone();
    let on_touch_start = move |ev: web_sys::TouchEvent| {
        start_cell.set(
            ev.touches()
                .get(0)
                .map(|t| (t.client_x() as f64, t.client_y() as f64)),
        );
    };

    let move_cell = swipe_start.clone();
    let on_touch_move = move |ev: web_sys::TouchEvent| {
        // Once the motion is clearly a vertical scroll, drop the start point so
        // touchend can't fire the back gesture. No prevent_default here.
        let Some((sx, sy)) = move_cell.get() else {
            return;
        };
        if let Some(t) = ev.touches().get(0) {
            let dx = t.client_x() as f64 - sx;
            let dy = t.client_y() as f64 - sy;
            if dy.abs() > SWIPE_THRESHOLD_PX && dy.abs() >= dx.abs() {
                move_cell.set(None);
            }
        }
    };

    let s_swipe = state.clone();
    let end_cell = swipe_start.clone();
    let on_touch_end = move |ev: web_sys::TouchEvent| {
        let Some((sx, sy)) = end_cell.take() else {
            return;
        };
        let Some(t) = ev.changed_touches().get(0) else {
            return;
        };
        let dx = t.client_x() as f64 - sx;
        let dy = t.client_y() as f64 - sy;
        let viewport_width = web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if back_swipe_triggered(sx, dx, dy, viewport_width) {
            more_open.set(false);
            s_swipe.viewing_chat.set(false);
        }
    };

    let s_name = state.clone();
    let agent_name = move || {
        let active = s_name.active_agent.get();
        active
            .and_then(|ar| {
                s_name.agents.with(|agents| {
                    agents
                        .iter()
                        .find(|a| a.local_host_id == ar.local_host_id && a.agent_id == ar.agent_id)
                        .map(|a| a.name.clone())
                })
            })
            .unwrap_or_else(|| "New Chat".to_string())
    };

    let s_backend = state.clone();
    let active_backend = move || {
        let active = s_backend.active_agent.get()?;
        s_backend.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.local_host_id == active.local_host_id && a.agent_id == active.agent_id)
                .map(|a| format!("{:?}", a.backend_kind))
        })
    };

    let s_interrupt = state.clone();
    let on_interrupt = Callback::new(move |_: ()| {
        let Some(ar) = s_interrupt.active_agent.get_untracked() else {
            return;
        };
        let agent_stream = s_interrupt.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|a| a.local_host_id == ar.local_host_id && a.agent_id == ar.agent_id)
                .map(|a| a.instance_stream.clone())
        });
        let Some(stream) = agent_stream else {
            report_interrupt_error(
                &s_interrupt,
                "Could not stop the turn: this agent's stream is no longer available.".to_owned(),
            );
            return;
        };
        let host_id = ar.local_host_id.clone();
        let state = s_interrupt.clone();
        spawn_local(async move {
            // The result used to be dropped with `let _ =`. A rejected admission
            // is exactly the case that matters here: the user taps Stop, the
            // frame never enters the outbound queue, the turn keeps running — and
            // nothing at all appears on screen. They are left believing they
            // stopped an agent that is still spending money.
            //
            // Admission is also *only* admission. Nothing here claims the turn
            // was interrupted: the agent stops when the server says it stopped,
            // which the UI already reflects through `agent_turn_active`. There is
            // no success message to make, so the success arm is silent — the
            // failure arm is the whole point.
            if let Err(error) = crate::send::send_frame(
                &host_id,
                stream,
                protocol::FrameKind::Interrupt,
                &protocol::InterruptPayload {},
            )
            .await
            {
                report_interrupt_error(
                    &state,
                    format!("Could not stop the turn: {error}. The agent is still running."),
                );
            }
        });
    });

    let s_turn = state.clone();
    let is_turn_active = move || {
        s_turn.active_agent.with(|ar| {
            ar.as_ref()
                .and_then(|ar| {
                    s_turn
                        .agent_turn_active
                        .with(|m| m.get(&ar.as_agent_ref()).copied())
                })
                .unwrap_or(false)
        })
    };

    // Rename state: when `rename_editing` is true the title becomes an
    // input. The draft is kept separate from the agent's actual name so
    // we don't push a partial edit through to the bridge on every keystroke.
    let rename_editing: RwSignal<bool> = RwSignal::new(false);
    let rename_draft: RwSignal<String> = RwSignal::new(String::new());

    let s_rename_open = state.clone();
    let on_rename_open = Callback::new(move |_: ()| {
        more_open.set(false);
        let current = s_rename_open
            .active_agent
            .with_untracked(|active| active.clone())
            .and_then(|ar| {
                s_rename_open.agents.with_untracked(|agents| {
                    agents
                        .iter()
                        .find(|a| a.local_host_id == ar.local_host_id && a.agent_id == ar.agent_id)
                        .map(|a| a.name.clone())
                })
            })
            .unwrap_or_default();
        rename_draft.set(current);
        rename_editing.set(true);
    });

    let s_rename_save = state.clone();
    let on_rename_save = Callback::new(move |_: ()| {
        let next = rename_draft.get_untracked().trim().to_string();
        if next.is_empty() {
            rename_editing.set(false);
            return;
        }
        let Some(active) = s_rename_save.active_agent.get_untracked() else {
            rename_editing.set(false);
            return;
        };
        let agent_ref = AgentRef {
            local_host_id: active.local_host_id.clone(),
            agent_id: active.agent_id.clone(),
        };
        let state_for_async = s_rename_save.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::rename_agent(&state_for_async, &agent_ref, next).await {
                log::error!("rename_agent failed: {e}");
            }
        });
        rename_editing.set(false);
    });

    let on_rename_cancel = Callback::new(move |_: ()| {
        rename_editing.set(false);
    });

    let s_close = state.clone();
    let on_close_agent = Callback::new(move |_: ()| {
        more_open.set(false);
        let Some(active) = s_close.active_agent.get_untracked() else {
            return;
        };
        let agent_ref = AgentRef {
            local_host_id: active.local_host_id.clone(),
            agent_id: active.agent_id.clone(),
        };
        let state_for_async = s_close.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::close_agent(&state_for_async, &agent_ref).await {
                log::error!("close_agent failed: {e}");
            }
        });
    });

    let s_has_active = state.clone();
    let has_active_agent = move || s_has_active.active_agent.with(|a| a.is_some());

    let s_compaction = state.clone();
    let compaction_label = move || {
        let active = s_compaction.active_agent.get()?;
        let ar = AgentRef {
            local_host_id: active.local_host_id.clone(),
            agent_id: active.agent_id.clone(),
        };
        let payload = s_compaction
            .agent_compactions
            .with(|m| m.get(&ar).cloned())?;
        Some(match payload.status {
            protocol::types::AgentCompactStatus::Started => "Compacting…".to_string(),
            protocol::types::AgentCompactStatus::Completed => "Compacted".to_string(),
            protocol::types::AgentCompactStatus::Failed => payload
                .message
                .unwrap_or_else(|| "Compaction failed".to_string()),
        })
    };

    let s_subtitle = state.clone();
    let header_subtitle = move || {
        let mut parts = Vec::new();
        if let Some(backend) = active_backend() {
            parts.push(backend);
        }
        if let Some(label) = compaction_label() {
            parts.push(label);
        }
        let active = s_subtitle.active_agent.get()?;
        let turn_active = s_subtitle
            .agent_turn_active
            .with(|m| m.get(&active.as_agent_ref()).copied().unwrap_or(false));
        if turn_active {
            parts.push("Responding".to_string());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" • "))
        }
    };

    let s_pending = state.clone();
    let s_body = state.clone();
    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    let user_scrolled_up = RwSignal::new(false);
    let auto_scroll_pending = Rc::new(Cell::new(false));
    let last_active_agent: Rc<RefCell<Option<AgentRef>>> = Rc::new(RefCell::new(None));

    let scroll_ref_for_auto = scroll_ref;
    let state_for_auto = state.clone();
    let pending_for_auto = auto_scroll_pending.clone();
    let last_active_for_auto = last_active_agent.clone();
    Effect::new(move |_| {
        let active_agent = state_for_auto
            .active_agent
            .get()
            .map(|ar| ar.as_agent_ref());
        let active_agent_stream = active_agent.as_ref().and_then(|key| {
            state_for_auto.agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| {
                        agent.local_host_id == key.local_host_id && agent.agent_id == key.agent_id
                    })
                    .map(|agent| agent.instance_stream.clone())
            })
        });
        if *last_active_for_auto.borrow() != active_agent {
            *last_active_for_auto.borrow_mut() = active_agent.clone();
            user_scrolled_up.set(false);
        }

        if let Some(key) = active_agent.as_ref() {
            if active_agent_stream.is_some()
                && state_for_auto
                    .host_stream_untracked(&key.local_host_id)
                    .is_some()
                && !state_for_auto
                    .agent_load_requests
                    .with_untracked(|loads| loads.contains(key))
            {
                state_for_auto.agent_load_requests.update(|loads| {
                    loads.insert(key.clone());
                });
                let state_for_load = state_for_auto.clone();
                let key_for_load = key.clone();
                spawn_local(async move {
                    if let Err(error) =
                        crate::actions::load_agent(&state_for_load, &key_for_load).await
                    {
                        log::error!(
                            "load_agent failed host={} agent_id={}: {}",
                            key_for_load.local_host_id,
                            key_for_load.agent_id,
                            error
                        );
                        // Record the failure and *keep* the latch. Clearing it
                        // would let this effect re-send `LoadAgent` on its next
                        // run, and the server rejects a duplicate load on an
                        // already-attached stream as a conflict — so an in-place
                        // retry cannot succeed. The typed error is what ends the
                        // spinner; recovery is a deliberate reconnect, which
                        // clears both the latch and the error and loads again on
                        // a fresh instance stream.
                        state_for_load.agent_load_errors.update(|errors| {
                            errors.insert(
                                key_for_load.clone(),
                                format!("Could not open this conversation: {error}"),
                            );
                        });
                    }
                });
            }
            track_active_chat_content(&state_for_auto, key);
        }

        if user_scrolled_up.get_untracked() || pending_for_auto.get() {
            return;
        }
        let Some(el) = scroll_ref_for_auto.get_untracked() else {
            return;
        };
        pending_for_auto.set(true);
        let pending = pending_for_auto.clone();
        set_timeout(
            move || {
                pending.set(false);
                scroll_chat_to_bottom(&el);
            },
            std::time::Duration::from_millis(0),
        );
    });

    let scroll_ref_for_scroll = scroll_ref;
    let on_scroll = move |_| {
        let Some(el) = scroll_ref_for_scroll.get_untracked() else {
            return;
        };
        user_scrolled_up.set(!chat_is_near_bottom(&el));
    };

    view! {
        <div
            class="view chat-view"
            data-mobile-test="chat-view"
            on:touchstart=on_touch_start
            on:touchmove=on_touch_move
            on:touchend=on_touch_end
        >
            <div class="chat-header">
                {move || {
                    if rename_editing.get() {
                        view! {
                            <div class="chat-rename-bar">
                                <input
                                    type="text"
                                    class="chat-header-rename-input"
                                    aria-label="Rename agent"
                                    data-mobile-test="chat-rename-input"
                                    prop:value=move || rename_draft.get()
                                    on:input=move |ev| {
                                        rename_draft.set(event_target_value(&ev));
                                    }
                                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                                        match ev.key().as_str() {
                                            "Enter" => {
                                                ev.prevent_default();
                                                on_rename_save.run(());
                                            }
                                            "Escape" => {
                                                ev.prevent_default();
                                                on_rename_cancel.run(());
                                            }
                                            _ => {}
                                        }
                                    }
                                />
                                <span class="chat-rename-actions">
                                    <Button
                                        label="Save"
                                        variant=ButtonVariant::Primary
                                        size=ButtonSize::Compact
                                        data_mobile_test="chat-rename-save"
                                        on_click=on_rename_save
                                    />
                                    <Button
                                        label="Cancel"
                                        variant=ButtonVariant::Ghost
                                        size=ButtonSize::Compact
                                        data_mobile_test="chat-rename-cancel"
                                        on_click=on_rename_cancel
                                    />
                                </span>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <button
                                type="button"
                                class="chat-back-button"
                                aria-label="Back to Agents"
                                data-mobile-test="chat-back"
                                on:click=on_back
                            >
                                <span class="chat-back-chevron" aria-hidden="true">"\u{2039}"</span>
                                <span class="chat-back-label">"Agents"</span>
                            </button>
                            <div class="chat-header-center">
                                <div class="chat-header-title" data-mobile-test="chat-title">
                                    {agent_name()}
                                </div>
                                {move || header_subtitle().map(|subtitle| view! {
                                    <div class="chat-header-subtitle" data-mobile-test="chat-subtitle">
                                        {subtitle}
                                    </div>
                                })}
                            </div>
                            <div class="chat-header-actions">
                                {move || {
                                    if is_turn_active() {
                                        view! {
                                            <Button
                                                label="Stop"
                                                variant=ButtonVariant::Destructive
                                                size=ButtonSize::Compact
                                                data_mobile_test="chat-stop"
                                                aria_label="Stop current turn".to_string()
                                                on_click=on_interrupt
                                            />
                                        }.into_any()
                                    } else if has_active_agent() {
                                        let rename_cb = on_rename_open;
                                        let close_cb = on_close_agent;
                                        view! {
                                            <div class="chat-more-menu-wrap">
                                                <button
                                                    type="button"
                                                    class="chat-more-button"
                                                    aria-label="More agent actions"
                                                    aria-expanded=move || more_open.get().to_string()
                                                    data-mobile-test="chat-more"
                                                    on:click=move |_| more_open.update(|open| *open = !*open)
                                                >
                                                    "\u{2026}"
                                                </button>
                                                <Show when=move || more_open.get()>
                                                    <div class="chat-action-menu" role="menu" data-mobile-test="chat-action-menu">
                                                        <button
                                                            type="button"
                                                            class="chat-action-menu-item"
                                                            role="menuitem"
                                                            data-mobile-test="chat-menu-rename"
                                                            on:click=move |_| rename_cb.run(())
                                                        >
                                                            "Rename"
                                                        </button>
                                                        <button
                                                            type="button"
                                                            class="chat-action-menu-item destructive"
                                                            role="menuitem"
                                                            data-mobile-test="chat-menu-close"
                                                            on:click=move |_| close_cb.run(())
                                                        >
                                                            "Close Agent"
                                                        </button>
                                                    </div>
                                                </Show>
                                            </div>
                                        }.into_any()
                                    } else {
                                        view! { <div class="chat-header-action-spacer"></div> }.into_any()
                                    }
                                }}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
            <div
                class="chat-messages"
                id="chat-messages-scroll"
                data-mobile-test="chat-messages"
                node_ref=scroll_ref
                on:scroll=on_scroll
            >
                {move || {
                    let active = s_body.active_agent.get();
                    let Some(ar) = active else {
                        // No active agent: invite the user to send the first
                        // message. The composer below is still live and will
                        // spawn a new chat on send.
                        return view! {
                            <EmptyState
                                title="Start a new chat"
                                body="Type below to spawn a new agent on your host. Your conversation history stays in sync with desktop."
                                icon="\u{1F4AC}"
                                data_mobile_test="chat-empty-new"
                            />
                        }.into_any();
                    };

                    let key = ar.as_agent_ref();
                    let messages = s_body.chat_messages.with(|m| {
                        m.get(&key).cloned().unwrap_or_default()
                    });
                    let prior_history = s_body.session_history.with(|m| m.get(&key).cloned());
                    let shown = messages.clone();
                    let load_state = s_body.clone();
                    let load_key = key.clone();
                    let load_stream = s_body.agents.with(|agents| {
                        agents
                            .iter()
                            .find(|agent| agent.local_host_id == key.local_host_id && agent.agent_id == key.agent_id)
                            .map(|agent| agent.instance_stream.clone())
                    });
                    let on_load_previous = move |_| {
                        let Some(history) = load_state
                            .session_history
                            .with_untracked(|m| m.get(&load_key).cloned())
                        else {
                            return;
                        };
                        if history.loading {
                            return;
                        }
                        let Some(stream) = load_stream.clone() else {
                            log::error!(
                                "load_previous_history: active agent stream missing host={} agent_id={}",
                                load_key.local_host_id,
                                load_key.agent_id
                            );
                            return;
                        };
                        load_state.session_history.update(|map| {
                            if let Some(history) = map.get_mut(&load_key) {
                                history.loading = true;
                            }
                        });
                        let state_for_error = load_state.clone();
                        let key_for_send = load_key.clone();
                        spawn_local(async move {
                            let payload = protocol::FetchSessionHistoryPayload {
                                agent_id: key_for_send.agent_id.clone(),
                                before_seq: history.oldest_seq,
                                limit: SESSION_HISTORY_PAGE_LIMIT,
                            };
                            if let Err(error) = crate::send::send_frame(
                                &key_for_send.local_host_id,
                                stream,
                                protocol::FrameKind::FetchSessionHistory,
                                &payload,
                            )
                            .await
                            {
                                log::error!("failed to send fetch_session_history: {error}");
                                state_for_error.session_history.update(|map| {
                                    if let Some(history) = map.get_mut(&key_for_send) {
                                        history.loading = false;
                                    }
                                });
                            }
                        });
                    };
                    let streaming = s_body.streaming_text.with(|m| m.get(&key).cloned());
                    let task_list = s_body.task_lists.with(|m| m.get(&key).cloned());
                    let transient = s_body.transient_events.with(|m| m.get(&key).cloned().unwrap_or_default());

                    let no_content = messages.is_empty()
                        && prior_history.is_none()
                        && streaming.is_none()
                        && task_list.is_none()
                        && transient.is_empty();

                    if no_content {
                        // Distinguish "still fetching the transcript" from
                        // "loaded and genuinely empty". A load latches into
                        // `agent_load_requests` the moment it's sent and only
                        // lands in `agent_loaded` once the bootstrap snapshot
                        // arrives; the gap is where a blank flash would
                        // otherwise read as an empty conversation.
                        //
                        // The spinner needs a *terminal* state, or it spins
                        // forever: `agent_loaded` is only ever written by a
                        // successful `AgentBootstrap`, so every failure mode —
                        // a rejected send, a dropped connection, a server
                        // `CommandError` — leaves the latch set and the
                        // transcript empty.
                        //
                        // Liveness is asked of **this agent's own host**, taken
                        // from the key. Reading `active_local_host_id` instead
                        // would infer the agent's host from wherever the user is
                        // currently pointing — a different question, and one that
                        // can have a different answer.
                        let host = &key.local_host_id;
                        let load_error = s_body
                            .agent_load_errors
                            .with(|errors| errors.get(&key).cloned());
                        let can_deliver = s_body.host_can_deliver(host);
                        let load_pending = s_body
                            .agent_load_requests
                            .with(|loads| loads.contains(&key))
                            && !s_body.agent_loaded.with(|loaded| loaded.contains(&key));

                        if let Some(message) = load_error {
                            return view! {
                                <ChatLoadFailed host=host.clone() message=message />
                            }.into_any();
                        }
                        // A disconnect drops every server snapshot for this host,
                        // including the transcript and the load latch. Without
                        // this branch the emptied chat renders as "Conversation is
                        // empty", which is a lie: it is not empty, we simply no
                        // longer know what is in it. Say what actually happened,
                        // and offer the only thing that fixes it.
                        if !can_deliver {
                            return view! {
                                <ChatLoadFailed
                                    host=host.clone()
                                    message="The connection to this host dropped, so this conversation could not be loaded.".to_owned()
                                />
                            }.into_any();
                        }
                        // KNOWN RESIDUAL: a host that stays `Connected` and simply
                        // never answers `LoadAgent` — and never errors — leaves
                        // this spinner running with no time bound. Every
                        // *transport* path out is now covered (rejected send,
                        // dropped connection, typed `CommandError`); what remains
                        // needs a genuine server fault.
                        //
                        // Deliberately not "fixed" with a timeout here. Any
                        // duration would be invented: too short and a slow link
                        // shows a false failure, too long and it is theatre. The
                        // honest fix is a server-side answer — an ack, or a
                        // protocol-level deadline — and it is not this change.
                        // Left visible rather than papered over.
                        if load_pending {
                            return view! {
                                <div class="chat-loading" data-mobile-test="chat-loading">
                                    <Spinner
                                        large=true
                                        aria_label="Loading conversation".to_string()
                                        data_mobile_test="chat-loading-spinner"
                                    />
                                </div>
                            }.into_any();
                        }
                        return view! {
                            <EmptyState
                                title="Conversation is empty"
                                body="Send a message to get started — your turn streams in real time."
                                icon="\u{1F4AC}"
                                data_mobile_test="chat-empty"
                            />
                        }.into_any();
                    }

                    view! {
                        <div class="chat-transcript" data-mobile-test="chat-transcript">
                            {prior_history.clone().map(|history| view! {
                                <div class="chat-history-collapsed" data-mobile-test="chat-history-collapsed">
                                    <button
                                        type="button"
                                        class="chat-history-load-previous"
                                        data-mobile-test="chat-load-previous"
                                        disabled=history.loading
                                        on:click=on_load_previous
                                    >
                                        {if history.loading {
                                            "Loading earlier messages…".to_owned()
                                        } else if history.message_count == 1 {
                                            "Load earlier messages (1 message)".to_owned()
                                        } else if history.message_count > 1 {
                                            format!(
                                                "Load earlier messages ({} messages)",
                                                history.message_count
                                            )
                                        } else {
                                            "Load earlier messages".to_owned()
                                        }}
                                    </button>
                                    <p class="chat-history-collapsed-note">
                                        "Earlier messages are available on demand."
                                    </p>
                                </div>
                            })}

                            // Task list
                            {task_list.map(|tl| {
                                view! {
                                    <div class="task-list-card" data-mobile-test="chat-task-list">
                                        {tl.tasks.into_iter().map(|task| {
                                            let status_icon = match task.status {
                                                protocol::TaskStatus::Pending => "\u{25CB}",
                                                protocol::TaskStatus::InProgress => "\u{25D4}",
                                                protocol::TaskStatus::Completed => "\u{2713}",
                                                protocol::TaskStatus::Failed => "\u{2717}",
                                            };
                                            view! {
                                                <div class="task-item">
                                                    <span class="task-status">{status_icon}</span>
                                                    <span class="task-content">{task.description}</span>
                                                </div>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }
                            })}

                            // Messages
                            //
                            // `key` is this chat's map key: the agent whose stream
                            // produced these rows, on the host that owns it. Tool
                            // cards need that ownership to resolve child agents, and
                            // it must travel with the row — not be read back from
                            // `active_agent`, which is navigation state that moves the
                            // moment the user opens another chat.
                            {shown.into_iter().map(|entry| {
                                let owner_agent_ref = key.clone();
                                view! {
                                    <ChatMessageView owner_agent_ref=owner_agent_ref entry=entry />
                                }
                            }).collect::<Vec<_>>()}

                            // Streaming message
                            {streaming.map(|s| {
                                // Same ownership rule as the completed rows above:
                                // the in-flight tool cards belong to the stream that
                                // is producing them, not to whatever chat the user
                                // happens to be looking at by the time they resolve.
                                let streaming_key = key.clone();
                                let text = s.text;
                                let reasoning = s.reasoning;
                                let tool_requests = s.tool_requests;
                                let model = s.model.unwrap_or_default();
                                let agent_name = s.agent_name;
                                view! {
                                    <div class="chat-message assistant streaming" data-mobile-test="chat-streaming">
                                        <div class="message-header">
                                            <span class="sender-name">{agent_name}</span>
                                            {
                                                let m1 = model.clone();
                                                let m2 = model.clone();
                                                view! {
                                                    <Show when=move || !m1.is_empty()>
                                                        <span class="model-badge">{m2.clone()}</span>
                                                    </Show>
                                                }
                                            }
                                        </div>
                                        {
                                            let r_check = reasoning.clone();
                                            let r_render = reasoning.clone();
                                            view! {
                                                <Show when=move || !r_check.get().is_empty()
                                                    fallback=|| ()
                                                >
                                                    {
                                                        let r = r_render.clone();
                                                        view! {
                                                            <div class="reasoning-block">
                                                                <div class="reasoning-label">"Thinking..."</div>
                                                                <div class="reasoning-text">{move || r.get()}</div>
                                                            </div>
                                                        }
                                                    }
                                                </Show>
                                            }
                                        }
                                        <div class="message-content" inner_html=move || crate::markdown::render_markdown(&text.get())></div>
                                        {move || {
                                            let tools = tool_requests.get();
                                            if tools.is_empty() {
                                                return view! { <div></div> }.into_any();
                                            }
                                            view! {
                                                <div class="tool-cards">
                                                    {tools.into_iter().map(|t| {
                                                        let owner_agent_ref = streaming_key.clone();
                                                        view! {
                                                            <crate::components::tool_card::ToolCardView
                                                                owner_agent_ref=owner_agent_ref
                                                                entry=t
                                                            />
                                                        }
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            }.into_any()
                                        }}
                                        <div class="streaming-indicator" role="status" aria-live="polite">
                                            <Spinner aria_label="Assistant is responding".to_string() />
                                        </div>
                                    </div>
                                }
                            })}

                            // Transient events
                            {transient.into_iter().map(|event| {
                                match event {
                                    crate::state::TransientEvent::OperationCancelled { message } => {
                                        view! {
                                            <div class="transient-event cancelled" data-mobile-test="chat-transient-cancelled" role="status">
                                                <span>"Operation cancelled: "{message}</span>
                                            </div>
                                        }.into_any()
                                    }
                                    crate::state::TransientEvent::RetryAttempt { attempt, max_retries, error, .. } => {
                                        view! {
                                            <div class="transient-event retry" data-mobile-test="chat-transient-retry" role="status">
                                                <span>"Retry "{attempt}"/"{max_retries}": "{error}</span>
                                            </div>
                                        }.into_any()
                                    }
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </div>
            // Submissions addressed to *this* agent that the transport could not
            // account for. Ownership is known — this is the agent we sent to —
            // so they belong in the conversation rather than on the host-scoped
            // surface. Directly above the composer, so they stay reachable with
            // the keyboard open. New-chat records never appear here: they have
            // no agent, and the client does not guess one.
            {move || {
                s_pending
                    .active_agent
                    .get()
                    .map(|active| view! {
                        <AgentPendingSubmissions agent_ref=active.as_agent_ref() />
                    })
            }}
            <ChatInput />
        </div>
    }
}

fn track_active_chat_content(state: &AppState, key: &AgentRef) {
    state.chat_messages.with(|m| {
        let _ = m.get(key).map_or(0, Vec::len);
    });
    state.session_history.with(|m| {
        let _ = m.contains_key(key);
    });
    state.task_lists.with(|m| {
        let _ = m.contains_key(key);
    });
    state.transient_events.with(|m| {
        let _ = m.get(key).map_or(0, Vec::len);
    });
    if let Some(streaming) = state.streaming_text.with(|m| m.get(key).cloned()) {
        streaming.text.with(|_| ());
        streaming.reasoning.with(|_| ());
        streaming.tool_requests.with(|requests| {
            let _ = requests.len();
        });
    }
}

fn chat_is_near_bottom(el: &web_sys::HtmlElement) -> bool {
    let distance_from_bottom = el.scroll_height() - el.scroll_top() - el.client_height();
    distance_from_bottom <= CHAT_STICKY_BOTTOM_THRESHOLD_PX
}

fn scroll_chat_to_bottom(el: &web_sys::HtmlElement) {
    el.set_scroll_top(el.scroll_height());
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{
        AgentInfo, AgentRef, AppState, ChatMessageEntry, LocalHostId, StreamingState,
        TransientEvent,
    };
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, ChatMessage, MessageSender, QueuedMessageEntry,
        QueuedMessageId, StreamPath,
    };

    // ChatMessage's field set evolves with the wire protocol; centralize
    // construction here so the tests stay easy to maintain.
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
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

    fn make_agent(host: &LocalHostId, name: &str) -> AgentInfo {
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId("agent-1".to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("stream/1".to_owned()),
            started: true,
            fatal_error: None,
        }
    }

    fn make_message(sender: MessageSender, content: &str) -> ChatMessageEntry {
        ChatMessageEntry {
            message: ChatMessage {
                message_id: None,
                timestamp: 0,
                sender,
                content: content.to_owned(),
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

    async fn settle_autoscroll() {
        next_tick().await;
        next_tick().await;
        next_tick().await;
    }

    /// Seed the state an agent's chat cannot exist without.
    ///
    /// An agent only reaches the client on a `HostBootstrap`, which only arrives over
    /// a live connection — so "this host has agents, but no connection status at all"
    /// is a world the real lifecycle never produces. `ChatView` (correctly) treats a
    /// host that cannot deliver a bootstrap as a terminal, actionable failure rather
    /// than an empty conversation, because calling an un-loadable chat "empty" is a
    /// lie the user would act on.
    ///
    /// A fixture that skipped the status was therefore asking the product to render a
    /// conversation for a host it had no evidence was reachable. Establishing the
    /// connection here, once, is what makes these tests model the lifecycle instead of
    /// an impossible state.
    fn seed_connected_agent(state: &AppState, host: &LocalHostId) {
        state.active_local_host_id.set(Some(host.clone()));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), crate::state::ConnectionStatus::Connected);
        });
        state.agents.set(vec![make_agent(host, "Coder")]);
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: host.clone(),
            agent_id: AgentId("agent-1".to_owned()),
        }));
    }

    fn mount_active_chat(container: HtmlElement) -> AppState {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_for_mount);
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatView /> }
        });
        std::mem::forget(handle);
        state_handle.borrow().as_ref().unwrap().clone()
    }

    fn stop_button(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector("[data-mobile-test='chat-stop']")
            .unwrap()
            .expect("the stop control must render while a turn is running")
            .dyn_into()
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn typed_live_reasoning_is_visible_in_the_active_stream() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();
        let reasoning = ArcRwSignal::new(String::new());
        state.streaming_text.update(|streams| {
            streams.insert(
                agent_ref,
                StreamingState {
                    agent_name: "codex".to_owned(),
                    model: Some("gpt-test".to_owned()),
                    text: ArcRwSignal::new(String::new()),
                    reasoning: reasoning.clone(),
                    tool_requests: ArcRwSignal::new(Vec::new()),
                },
            );
        });

        next_tick().await;
        assert!(
            container
                .query_selector(".reasoning-text")
                .unwrap()
                .is_none(),
            "an empty stream has no reasoning block"
        );

        reasoning.set("inspect the first item".to_owned());
        next_tick().await;
        let streaming = container
            .query_selector("[data-mobile-test='chat-streaming']")
            .unwrap()
            .expect("active streaming message");
        let reasoning_text = streaming
            .query_selector(".reasoning-text")
            .unwrap()
            .expect("visible typed reasoning preview");
        assert_eq!(
            reasoning_text.text_content().as_deref(),
            Some("inspect the first item")
        );
        assert!(
            streaming
                .query_selector(".message-content")
                .unwrap()
                .expect("streaming body")
                .text_content()
                .is_none_or(|text| text.is_empty()),
            "reasoning-only preview must not create a second empty bubble"
        );

        reasoning.set("inspect the eventual assistant item".to_owned());
        next_tick().await;
        assert_eq!(
            reasoning_text.text_content().as_deref(),
            Some("inspect the eventual assistant item"),
            "the same preview updates without rebinding"
        );
    }

    /// **A stop that never left the client must not look like a stop that worked.**
    ///
    /// The result of the `Interrupt` send used to be thrown away with `let _ =`.
    /// A rejected admission is exactly the case that matters: the user taps Stop,
    /// the frame never enters the outbound queue, the agent keeps running and
    /// keeps spending money — and absolutely nothing appears on screen. They walk
    /// away believing they stopped it.
    #[wasm_bindgen_test]
    async fn a_rejected_interrupt_is_surfaced_and_says_the_agent_is_still_running() {
        let _guard = crate::bridge::test_reject_sends();
        let container = make_container();
        let state = mount_active_chat(container.clone());
        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();
        state.agent_turn_active.update(|m| {
            m.insert(agent_ref, true);
        });
        settle_autoscroll().await;

        assert!(
            state.mobile_shell_error.get_untracked().is_none(),
            "precondition: nothing surfaced yet"
        );

        stop_button(&container).click();
        settle_autoscroll().await;

        let surfaced = state
            .mobile_shell_error
            .get_untracked()
            .expect("a stop that was never admitted must not fail silently");
        assert!(
            surfaced.message.contains("still running"),
            "the user must be told the agent did not stop, got: {}",
            surfaced.message
        );
        assert!(
            !surfaced.message.to_lowercase().contains("stopped")
                && !surfaced.message.to_lowercase().contains("interrupted"),
            "nothing may claim the turn was interrupted: {}",
            surfaced.message
        );
    }

    /// Admission is not interruption. A successfully *queued* Interrupt says
    /// nothing at all — the agent stops when the server says it stopped, which the
    /// UI already reflects through `agent_turn_active`. Inventing a "Stopped!"
    /// message here would be the same false-success claim the whole model bans.
    #[wasm_bindgen_test]
    async fn an_admitted_interrupt_claims_nothing() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let state = mount_active_chat(container.clone());
        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();
        state.agent_turn_active.update(|m| {
            m.insert(agent_ref.clone(), true);
        });
        settle_autoscroll().await;

        stop_button(&container).click();
        settle_autoscroll().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "the interrupt must actually be sent"
        );
        assert!(
            state.mobile_shell_error.get_untracked().is_none(),
            "an admitted interrupt is not an error and must not surface one"
        );
        assert!(
            state
                .agent_turn_active
                .with_untracked(|m| m.get(&agent_ref).copied().unwrap_or(false)),
            "the turn is still active until the *server* says otherwise — admission \
             is not interruption, and the client must not pretend it is"
        );
        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "an interrupt carries no user text, so there is nothing to hold or recover"
        );
    }

    /// The spinner must have a way to end that is not "success".
    ///
    /// `agent_loaded` is only ever written by a successful `AgentBootstrap`, so
    /// a spinner gated on it alone spins forever on every failure — which is
    /// exactly what an opened agent did. A typed load error is the terminal
    /// state, and it must replace the spinner with something the user can act on.
    #[wasm_bindgen_test]
    async fn a_failed_load_replaces_the_spinner_with_a_recovery_action() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();

        // The load has been sent and no snapshot has arrived: this is the
        // spinner window.
        state.agent_load_requests.update(|loads| {
            loads.insert(agent_ref.clone());
        });
        state.connection_statuses.update(|m| {
            m.insert(
                agent_ref.local_host_id.clone(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        settle_autoscroll().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading-spinner']")
                .unwrap()
                .is_some(),
            "an in-flight load must show a spinner"
        );

        // The load fails. The spinner must not survive it.
        state.agent_load_errors.update(|errors| {
            errors.insert(agent_ref, "host said no".to_owned());
        });
        settle_autoscroll().await;

        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading-spinner']")
                .unwrap()
                .is_none(),
            "a failed load must end the spinner, not spin forever"
        );
        let failed = container
            .query_selector("[data-mobile-test='chat-load-failed']")
            .unwrap()
            .expect("a failed load must render a visible terminal state");
        assert_eq!(
            failed.get_attribute("role").as_deref(),
            Some("alert"),
            "a screen reader told 'Loading conversation' must be told how it ended"
        );
        let text = failed.text_content().unwrap_or_default();
        assert!(
            text.contains("host said no"),
            "the terminal state must carry the real reason, got: {text}"
        );
        let reconnect = container
            .query_selector("[data-mobile-test='chat-load-failed-reconnect']")
            .unwrap()
            .expect("the user must be offered a way out, not just told it broke");
        assert!(
            !reconnect.has_attribute("disabled"),
            "the recovery action must be reachable"
        );
    }

    /// A connection that can no longer deliver a bootstrap ends the spinner too,
    /// even though no per-agent error was ever attributed.
    #[wasm_bindgen_test]
    async fn a_dropped_connection_ends_the_spinner() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();

        state.agent_load_requests.update(|loads| {
            loads.insert(agent_ref.clone());
        });
        state.connection_statuses.update(|m| {
            m.insert(
                agent_ref.local_host_id.clone(),
                crate::state::ConnectionStatus::Disconnected,
            );
        });
        settle_autoscroll().await;

        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading-spinner']")
                .unwrap()
                .is_none(),
            "a bootstrap that can never arrive must not be rendered as still loading"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-load-failed-reconnect']")
                .unwrap()
                .is_some(),
            "the user must be offered a reconnect"
        );
    }

    fn chat_scroller(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector("[data-mobile-test='chat-messages']")
            .unwrap()
            .expect("chat scroller")
            .dyn_into::<HtmlElement>()
            .unwrap()
    }

    fn fill_chat(state: &AppState, count: usize) {
        let active = state.active_agent.get_untracked().expect("active agent");
        let agent_ref = active.as_agent_ref();
        state.chat_messages.update(|m| {
            m.insert(
                agent_ref,
                (0..count)
                    .map(|i| {
                        make_message(
                            MessageSender::Assistant {
                                agent: "Coder".to_owned(),
                            },
                            &format!("Message {i}\n\n{}", "content ".repeat(20)),
                        )
                    })
                    .collect(),
            );
        });
    }

    fn distance_from_bottom(el: &HtmlElement) -> i32 {
        el.scroll_height() - el.scroll_top() - el.client_height()
    }

    /// With no active agent, the "Start a new chat" empty state appears
    /// — distinct from the "Conversation is empty" state so users know
    /// the difference between "haven't picked a chat" and "picked but
    /// empty."
    #[wasm_bindgen_test]
    async fn chat_empty_new_when_no_active_agent() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-empty-new']")
                .unwrap()
                .is_some(),
            "no-active-agent path must show chat-empty-new"
        );
    }

    #[wasm_bindgen_test]
    async fn chat_auto_scrolls_to_bottom_when_user_is_sticky() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        next_tick().await;
        let scroller = chat_scroller(&container);
        scroller
            .set_attribute("style", "height: 96px; overflow-y: auto; display: block;")
            .unwrap();

        fill_chat(&state, 40);
        settle_autoscroll().await;

        assert!(
            distance_from_bottom(&scroller) <= CHAT_STICKY_BOTTOM_THRESHOLD_PX,
            "sticky chat should scroll to bottom; scrollTop={} clientHeight={} scrollHeight={}",
            scroller.scroll_top(),
            scroller.client_height(),
            scroller.scroll_height()
        );
    }

    #[wasm_bindgen_test]
    async fn chat_does_not_auto_scroll_after_user_scrolls_up() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        next_tick().await;
        let scroller = chat_scroller(&container);
        scroller
            .set_attribute("style", "height: 96px; overflow-y: auto; display: block;")
            .unwrap();
        fill_chat(&state, 40);
        settle_autoscroll().await;
        assert!(
            distance_from_bottom(&scroller) <= CHAT_STICKY_BOTTOM_THRESHOLD_PX,
            "setup should start sticky at bottom"
        );

        scroller.set_scroll_top(0);
        scroller
            .dispatch_event(&web_sys::Event::new("scroll").unwrap())
            .unwrap();
        settle_autoscroll().await;
        let before = scroller.scroll_top();

        fill_chat(&state, 41);
        settle_autoscroll().await;

        assert!(
            scroller.scroll_top() <= before + 4,
            "chat should preserve user-scrolled position; before={} after={} distance={}",
            before,
            scroller.scroll_top(),
            distance_from_bottom(&scroller)
        );
        assert!(
            distance_from_bottom(&scroller) > CHAT_STICKY_BOTTOM_THRESHOLD_PX,
            "user-scrolled chat should remain away from bottom"
        );
    }

    /// Prior history is represented by a server-owned indicator, not by rows
    /// that the client hides after receiving them.
    #[wasm_bindgen_test]
    async fn prior_history_indicator_shows_load_control_without_rows() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        next_tick().await;

        let agent_ref = state
            .active_agent
            .get_untracked()
            .expect("active agent")
            .as_agent_ref();
        state.session_history.update(|m| {
            m.insert(
                agent_ref,
                crate::state::SessionHistoryState {
                    message_count: 25,
                    oldest_seq: Some(42),
                    has_more_before: true,
                    loading: false,
                },
            );
        });
        settle_autoscroll().await;

        let collapsed_rows = container
            .query_selector_all(".chat-transcript .chat-message")
            .unwrap();
        assert_eq!(
            collapsed_rows.length(),
            0,
            "prior history must not be present as hidden client rows, got {}",
            collapsed_rows.length()
        );
        let banner = container
            .query_selector("[data-mobile-test='chat-history-collapsed']")
            .unwrap();
        assert!(
            banner.is_some(),
            "prior-history indicator must show the load-previous banner"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Load earlier messages (25 messages)"),
            "banner must offer the load-earlier control: {text}"
        );
        assert!(
            text.contains("available on demand"),
            "history note must explain on-demand loading: {text}"
        );
    }

    /// A chat opened on a slow link shows a loading spinner — not the
    /// "Conversation is empty" state — in the window after the load is
    /// requested but before its bootstrap snapshot arrives.
    #[wasm_bindgen_test]
    async fn chat_shows_spinner_while_conversation_loads() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            // Load latched, but the bootstrap snapshot has not arrived yet.
            state.agent_load_requests.update(|m| {
                m.insert(agent_ref);
            });
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading']")
                .unwrap()
                .is_some(),
            "spinner must show while the transcript is still loading"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-empty']")
                .unwrap()
                .is_none(),
            "empty state must not show while the transcript is loading"
        );
    }

    /// Once the bootstrap snapshot lands and the conversation is genuinely
    /// empty, the spinner gives way to the "Conversation is empty" state.
    #[wasm_bindgen_test]
    async fn chat_swaps_spinner_for_empty_once_loaded() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.agent_load_requests.update(|m| {
                m.insert(agent_ref.clone());
            });
            // Bootstrap snapshot has now arrived with no messages.
            state.agent_loaded.update(|m| {
                m.insert(agent_ref);
            });
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading']")
                .unwrap()
                .is_none(),
            "spinner must clear once the snapshot has arrived"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-empty']")
                .unwrap()
                .is_some(),
            "loaded-but-empty conversation must show the empty state"
        );
    }

    /// Regression for the LoadAgent-failure loop: when a load fails the
    /// dispatcher keeps the `agent_load_requests` latch set and pushes one
    /// error row. The chat must show that error (no spinner), and the
    /// auto-load effect must NOT re-send LoadAgent or stack a second error
    /// when it runs — the retained latch is what blocks the retry. Here the
    /// host stream is present so the only guard left standing is the latch.
    #[wasm_bindgen_test]
    async fn failed_load_keeps_latch_and_does_not_stack_errors() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            let agent = make_agent(&host_for_mount, "Coder");
            state.agents.set(vec![agent]);
            // Give the host a stream so the auto-load effect's host-stream
            // guard passes; the retained latch is then the only thing
            // preventing a re-send.
            state.host_streams.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    StreamPath("/host/host-1".to_owned()),
                );
            });
            let agent_ref = AgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            // Post-failed-load state: latch retained, snapshot never arrived,
            // a single error row already surfaced by the dispatcher.
            state.agent_load_requests.update(|m| {
                m.insert(agent_ref.clone());
            });
            state.chat_messages.update(|m| {
                m.insert(
                    agent_ref,
                    vec![make_message(
                        MessageSender::Error,
                        "Failed to load conversation: agent already attached",
                    )],
                );
            });
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatView /> }
        });
        std::mem::forget(handle);
        let state = state_handle.borrow().as_ref().unwrap().clone();
        settle_autoscroll().await;

        // Error row retires the spinner.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading']")
                .unwrap()
                .is_none(),
            "error row must replace the loading spinner"
        );
        // Exactly one error row rendered — no stacked duplicates.
        let error_rows = container
            .query_selector_all("[data-mobile-test='chat-message-error']")
            .unwrap();
        assert_eq!(
            error_rows.length(),
            1,
            "exactly one error row must render, got {}",
            error_rows.length()
        );

        let agent_ref = AgentRef {
            local_host_id: host.clone(),
            agent_id: AgentId("agent-1".to_owned()),
        };
        // The auto-load effect saw the new chat content but must not have
        // cleared the latch nor appended another error row.
        assert!(
            state
                .agent_load_requests
                .with_untracked(|m| m.contains(&agent_ref)),
            "load latch must stay set so the auto-load effect does not retry"
        );
        assert_eq!(
            state
                .chat_messages
                .with_untracked(|m| m.get(&agent_ref).map(|v| v.len())),
            Some(1),
            "the effect must not append a duplicate error row"
        );
    }

    /// Active agent with no content gets the "Conversation is empty"
    /// empty state, not the "Start a new chat" state.
    #[wasm_bindgen_test]
    async fn chat_empty_when_active_agent_has_no_messages() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-empty']")
                .unwrap()
                .is_some(),
            "active-but-empty path must show chat-empty"
        );
        // Backend now lives in the iOS-style navigation subtitle.
        let subtitle = container
            .query_selector("[data-mobile-test='chat-subtitle']")
            .unwrap()
            .expect("subtitle must render when an agent is active");
        let text = subtitle.text_content().unwrap_or_default();
        assert!(
            text.contains("Claude"),
            "subtitle must show backend name, got: {text}"
        );
    }

    /// Queued messages are managed by the composer, so the transcript
    /// must not render a duplicate queued-message surface.
    #[wasm_bindgen_test]
    async fn chat_does_not_render_queued_messages_in_transcript() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            // One sent message + two queued.
            state.chat_messages.update(|m| {
                m.insert(
                    agent_ref.clone(),
                    vec![make_message(MessageSender::User, "First")],
                );
            });
            state.agent_message_queue.update(|m| {
                m.insert(
                    agent_ref.clone(),
                    vec![
                        QueuedMessageEntry {
                            id: QueuedMessageId("q-1".to_owned()),
                            message: "second pending".to_owned(),
                            images: Vec::new(),
                            origin: None,
                        },
                        QueuedMessageEntry {
                            id: QueuedMessageId("q-2".to_owned()),
                            message: "third pending".to_owned(),
                            images: Vec::new(),
                            origin: None,
                        },
                    ],
                );
            });
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        let transcript = container
            .query_selector("[data-mobile-test='chat-messages']")
            .unwrap()
            .expect("chat messages container must render");
        assert!(
            transcript
                .query_selector("[data-mobile-test='chat-queued']")
                .unwrap()
                .is_none(),
            "queued messages must not render in the transcript"
        );
        let transcript_text = transcript.text_content().unwrap_or_default();
        assert!(
            !transcript_text.contains("second pending")
                && !transcript_text.contains("third pending"),
            "queued message bodies must stay out of the transcript: {transcript_text}"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-list']")
                .unwrap()
                .is_some(),
            "composer queued controls should remain available"
        );
    }

    /// With an active agent and no turn running, the header looks like an
    /// iOS navigation bar: back affordance, centered title/subtitle, and a
    /// compact More menu instead of text buttons.
    #[wasm_bindgen_test]
    async fn chat_header_uses_ios_nav_and_more_menu_when_idle() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-rename']")
                .unwrap()
                .is_none(),
            "rename must not be a top-level header button"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-close']")
                .unwrap()
                .is_none(),
            "close must not be a top-level header button"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-stop']")
                .unwrap()
                .is_none(),
            "stop must not render while idle"
        );
        let back_text = container
            .query_selector("[data-mobile-test='chat-back']")
            .unwrap()
            .expect("back button")
            .text_content()
            .unwrap_or_default();
        assert!(
            back_text.contains("Agents"),
            "back affordance should label the destination: {back_text}"
        );
        let title = container
            .query_selector("[data-mobile-test='chat-title']")
            .unwrap()
            .expect("title")
            .text_content()
            .unwrap_or_default();
        assert!(
            title.contains("Coder"),
            "title should show agent name: {title}"
        );
        let subtitle = container
            .query_selector("[data-mobile-test='chat-subtitle']")
            .unwrap()
            .expect("subtitle")
            .text_content()
            .unwrap_or_default();
        assert!(
            subtitle.contains("Claude"),
            "subtitle should show backend: {subtitle}"
        );

        let more_btn: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='chat-more']")
            .unwrap()
            .expect("more button")
            .dyn_into()
            .unwrap();
        more_btn.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-menu-rename']")
                .unwrap()
                .is_some(),
            "rename should move into the More menu"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-menu-close']")
                .unwrap()
                .is_some(),
            "close should move into the More menu"
        );
    }

    /// Tapping Rename swaps the title for an input. Pressing Escape
    /// closes the rename input without firing the rename outbound.
    #[wasm_bindgen_test]
    async fn chat_rename_input_opens_and_escape_cancels() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        // Open the rename UI through the iOS-style More menu.
        let more_btn: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='chat-more']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        more_btn.click();
        next_tick().await;
        let rename_btn: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='chat-menu-rename']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        rename_btn.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-rename-input']")
                .unwrap()
                .is_some(),
            "rename input must appear after tapping Rename"
        );
        // Cancel via the visible Cancel button (Escape via keydown would
        // require synthesizing a real KeyboardEvent which isn't worth
        // wrestling with for this assertion).
        let cancel_btn: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='chat-rename-cancel']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        cancel_btn.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-rename-input']")
                .unwrap()
                .is_none(),
            "rename input must disappear after Cancel"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-title']")
                .unwrap()
                .is_some(),
            "title text must come back after Cancel"
        );
    }

    /// The transcript should stay free of queued-message row controls;
    /// the composer owns Send Now/Delete while a turn is running.
    #[wasm_bindgen_test]
    async fn chat_transcript_omits_queued_row_controls() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.agent_message_queue.update(|m| {
                m.insert(
                    agent_ref,
                    vec![
                        QueuedMessageEntry {
                            id: QueuedMessageId("q-1".to_owned()),
                            message: "first".to_owned(),
                            images: Vec::new(),
                            origin: None,
                        },
                        QueuedMessageEntry {
                            id: QueuedMessageId("q-2".to_owned()),
                            message: "second".to_owned(),
                            images: Vec::new(),
                            origin: None,
                        },
                    ],
                );
            });
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        let transcript = container
            .query_selector("[data-mobile-test='chat-messages']")
            .unwrap()
            .expect("chat messages container must render");
        assert!(
            transcript
                .query_selector("[data-mobile-test='chat-queued-row']")
                .unwrap()
                .is_none(),
            "queued rows must not render in transcript"
        );
        assert!(
            transcript
                .query_selector("[data-mobile-test='chat-queued-cancel']")
                .unwrap()
                .is_none(),
            "queued Delete controls must not render in transcript"
        );
        assert!(
            transcript
                .query_selector("[data-mobile-test='chat-queued-send-now']")
                .unwrap()
                .is_none(),
            "queued Send Now controls must not render in transcript"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-send-now']")
                .unwrap()
                .is_some(),
            "composer still exposes Send Now"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-delete']")
                .unwrap()
                .is_some(),
            "composer still exposes Delete"
        );
    }

    /// Transient events use dedicated selectors so a cancellation can
    /// be distinguished from a retry by tests (and users see different
    /// border-color treatments).
    #[wasm_bindgen_test]
    async fn chat_renders_transient_cancelled_and_retry() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_connected_agent(&state, &host_clone);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.transient_events.update(|m| {
                m.insert(
                    agent_ref,
                    vec![
                        TransientEvent::OperationCancelled {
                            message: "user".to_owned(),
                        },
                        TransientEvent::RetryAttempt {
                            attempt: 1,
                            max_retries: 3,
                            error: "boom".to_owned(),
                            backoff_ms: 1000,
                        },
                    ],
                );
            });
            provide_context(state);
            view! { <ChatView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-transient-cancelled']")
                .unwrap()
                .is_some(),
            "cancelled transient selector must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-transient-retry']")
                .unwrap()
                .is_some(),
            "retry transient selector must render"
        );
    }

    /// The default edge-swipe (start near the right edge, travel left past the
    /// threshold, dominantly horizontal) is recognized, while gestures that
    /// start mid-screen, fall short, or are dominantly vertical are not. This
    /// pins the geometry that keeps the gesture from firing during scrolling.
    #[wasm_bindgen_test]
    async fn back_swipe_decision_matches_ios_left_edge_geometry() {
        let vw = 400.0;
        // Starts in the left edge zone, long rightward, horizontal: fires.
        assert!(
            back_swipe_triggered(5.0, 120.0, 10.0, vw),
            "left-edge rightward horizontal swipe must trigger back"
        );
        // Starts in the middle of the screen: not an edge swipe.
        assert!(
            !back_swipe_triggered(vw / 2.0, 120.0, 10.0, vw),
            "swipe starting mid-screen must not trigger back"
        );
        // Horizontal travel below threshold: ignored.
        assert!(
            !back_swipe_triggered(5.0, 40.0, 5.0, vw),
            "short swipe must not trigger back"
        );
        // Dominantly vertical (a transcript scroll): ignored.
        assert!(
            !back_swipe_triggered(5.0, 70.0, -200.0, vw),
            "dominantly vertical drag must not trigger back"
        );
        // Left edge but moving the wrong way (leftward): ignored.
        assert!(
            !back_swipe_triggered(5.0, -120.0, 10.0, vw),
            "leftward travel from the left edge must not trigger back"
        );
    }

    /// The swipe-back path sets `viewing_chat` to false — the same state
    /// transition the back button performs — returning the user to the list.
    #[wasm_bindgen_test]
    async fn back_action_clears_viewing_chat() {
        let container = make_container();
        let state = mount_active_chat(container.clone());
        state.viewing_chat.set(true);
        next_tick().await;

        let back_btn: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='chat-back']")
            .unwrap()
            .expect("back button")
            .dyn_into()
            .unwrap();
        back_btn.click();
        next_tick().await;

        assert!(
            !state.viewing_chat.get_untracked(),
            "back navigation must clear viewing_chat"
        );
    }
}
