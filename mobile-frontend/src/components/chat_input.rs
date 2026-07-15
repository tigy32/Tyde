use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::bridge::Accepted;
use crate::send::SendFrameError;
use crate::state::{
    AgentRef, AppState, LocalHostId, PendingSubmission, PendingSubmissionState, SubmissionTarget,
};

const CHAT_INPUT_MIN_HEIGHT_PX: i32 = 36;
const CHAT_INPUT_MAX_HEIGHT_PX: i32 = 132;

/// Visually hidden, still announced. Inline because this is an accessibility
/// invariant of the composer, not a theming choice.
const VISUALLY_HIDDEN: &str = "position:absolute;width:1px;height:1px;padding:0;margin:-1px;\
     overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0;";

/// What the composer announces when a submission is admitted.
///
/// Sighted users see the composer empty itself. A screen-reader user would
/// otherwise just find their text gone, so the move is announced politely.
///
/// It says **queued**, not sent: admission means the frame entered this
/// connection's outbound queue, and the client has no basis for claiming the
/// host received it. A `polite` status, never an alert — the happy path must not
/// interrupt, and it leaves no artifact to dismiss.
const QUEUED_ANNOUNCEMENT: &str = "Message queued to send.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueuedRowRef {
    agent_ref: AgentRef,
    id: protocol::QueuedMessageId,
}

/// One outbound user submission as the composer sees it: where it is going, and
/// what the user put in it.
///
/// Bundled rather than passed as loose parameters — the destination, the text,
/// and the attachments travel together or they are not a submission, and
/// splitting them into a seven-argument call is what invites a caller to pass
/// the host of one message with the text of another.
struct OutboundSubmission {
    local_host_id: LocalHostId,
    target: SubmissionTarget,
    text: String,
    images: Vec<protocol::ImageData>,
}

/// The composer's own handles: the textarea, the live region that announces a
/// move, and the in-flight latch.
///
/// `Copy`, so it can be handed to a `spawn_local` future without cloning
/// ceremony at every call site.
#[derive(Clone, Copy)]
struct Composer {
    textarea: NodeRef<leptos::html::Textarea>,
    announcement: RwSignal<String>,
    /// Set while a submission is unsettled.
    ///
    /// This is a real guard, not a side effect. Before the fix, the composer was
    /// cleared *before* the send was awaited, so a second tap read an empty box
    /// and fell out of `if text.is_empty()`. Preserving the user's text across
    /// the in-flight window — the whole point — removed that accident. Nothing
    /// then stopped a double-tap from emitting two `SpawnAgent` frames, which is
    /// two agents, two backend sessions, and two paid turns.
    ///
    /// It does not rely on the send resolving in the same microtask. It holds
    /// even when the send genuinely yields.
    submitting: RwSignal<bool>,
}

impl Composer {
    fn new() -> Self {
        Self {
            textarea: NodeRef::new(),
            announcement: RwSignal::new(String::new()),
            submitting: RwSignal::new(false),
        }
    }

    fn is_busy(&self) -> bool {
        self.submitting.get_untracked()
    }

    fn begin(&self) {
        self.submitting.set(true);
    }

    fn finish(&self) {
        self.submitting.set(false);
    }

    /// Empty the composer. Called only from [`settle_submission`], and only once
    /// the text has a holder.
    fn clear(&self, state: &AppState) {
        state.chat_input.set(String::new());
        if let Some(textarea) = self.textarea.get_untracked() {
            textarea.set_value("");
            resize_chat_input(&textarea);
        }
    }
}

fn queued_message_preview(entry: &protocol::QueuedMessageEntry) -> String {
    let mut preview = entry.message.trim().to_string();
    if preview.is_empty() {
        preview = match entry.images.len() {
            0 => "Queued message".to_owned(),
            1 => "Image attachment".to_owned(),
            count => format!("{count} image attachments"),
        };
    } else if !entry.images.is_empty() {
        let suffix = if entry.images.len() == 1 {
            "image"
        } else {
            "images"
        };
        preview.push_str(&format!(" (+{} {suffix})", entry.images.len()));
    }

    let chars: Vec<char> = preview.chars().collect();
    if chars.len() > 80 {
        chars[..80].iter().collect::<String>() + "…"
    } else {
        preview
    }
}

fn active_agent_stream(
    state: &AppState,
    active: &crate::state::ActiveAgentRef,
) -> Option<protocol::StreamPath> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.local_host_id == active.local_host_id && a.agent_id == active.agent_id)
            .map(|a| a.instance_stream.clone())
    })
}

fn active_agent_is_running_tracked(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get() else {
        return false;
    };
    let agent_ref = active.as_agent_ref();
    if state
        .agent_turn_active
        .with(|turns| turns.get(&agent_ref).copied().unwrap_or(false))
    {
        return true;
    }
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && !agent.started
                && agent.fatal_error.is_none()
        })
    })
}

/// True when the active agent has reported a backend session id, which is
/// required to fork via "Fork + send".
fn active_agent_has_session_id_tracked(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get() else {
        return false;
    };
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && agent.session_id.is_some()
        })
    })
}

#[component]
fn QueuedMessageControlRow(row: QueuedRowRef) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let preview_agent = row.agent_ref.clone();
    let preview_id = row.id.clone();
    let preview_state = state.clone();
    let preview = move || {
        preview_state.agent_message_queue.with(|queues| {
            queues
                .get(&preview_agent)
                .and_then(|entries| entries.iter().find(|entry| entry.id == preview_id))
                .map(queued_message_preview)
                .unwrap_or_default()
        })
    };

    let send_now_agent = row.agent_ref.clone();
    let send_now_id = row.id.clone();
    let send_now_state = state.clone();
    let on_send_now = move |_| {
        let state = send_now_state.clone();
        let agent_ref = send_now_agent.clone();
        let id = send_now_id.clone();
        spawn_local(async move {
            if let Err(error) =
                crate::actions::send_queued_message_now(&state, &agent_ref, id).await
            {
                report_send_error(
                    &state,
                    format!("Failed to send queued message now: {error}"),
                );
            }
        });
    };

    let delete_agent = row.agent_ref;
    let delete_id = row.id;
    let delete_state = state.clone();
    let on_delete = move |_| {
        let state = delete_state.clone();
        let agent_ref = delete_agent.clone();
        let id = delete_id.clone();
        spawn_local(async move {
            if let Err(error) = crate::actions::cancel_queued_message(&state, &agent_ref, id).await
            {
                report_send_error(&state, format!("Failed to delete queued message: {error}"));
            }
        });
    };

    view! {
        <div class="chat-input-queued-row" data-mobile-test="chat-input-queued-row">
            <span class="chat-input-queued-preview">{preview}</span>
            <button
                type="button"
                class="chat-input-queued-action chat-input-queued-send-now"
                aria-label="Send queued message now"
                data-mobile-test="chat-input-queued-send-now"
                on:click=on_send_now
            >
                "Send Now"
            </button>
            <button
                type="button"
                class="chat-input-queued-action chat-input-queued-delete"
                aria-label="Delete queued message"
                data-mobile-test="chat-input-queued-delete"
                on:click=on_delete
            >
                "Delete"
            </button>
        </div>
    }
}

/// Mobile chat composer.
///
/// Primary button label follows the state matrix: "Send" when idle, "Queue"
/// when a turn is running and there is draft text, "Cancel" when running with
/// an empty composer. The caret is always rendered but disabled when the
/// dropdown would be empty. The dropdown carries secondary actions only:
/// "Steer" and "Cancel" when running+input; "Fork + send" when a forkable
/// session exists and there is draft text.
#[component]
pub fn ChatInput() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let composer = Composer::new();
    let textarea_ref = composer.textarea;
    let submitting = composer.submitting;

    let do_send = {
        let state = state.clone();
        move || {
            // The composer now *keeps* the user's text for the whole in-flight
            // window — that is the entire point of the fix. It also means the old
            // double-send guard is gone: clearing the box early used to make a
            // second tap read empty text and short-circuit. That was never a
            // guard, it was a side effect, and a second `SpawnAgent` costs a
            // second agent and a second paid turn. Guard it explicitly.
            if composer.is_busy() {
                return;
            }
            let text = state.chat_input.get_untracked().trim().to_string();
            if text.is_empty() {
                return;
            }

            let state = state.clone();
            // Resolve the destination *before* anything moves, so a target we
            // cannot address fails with the composer still full.
            let active_target = match state.active_agent.get_untracked() {
                Some(active) => {
                    let Some(stream) = active_agent_stream(&state, &active) else {
                        report_send_error(
                            &state,
                            "Failed to send message: agent stream not found".into(),
                        );
                        return;
                    };
                    Some((active, stream))
                }
                None => None,
            };

            let host = match &active_target {
                Some((active, _)) => active.local_host_id.clone(),
                None => match state.active_local_host_id.get_untracked() {
                    Some(host) => host,
                    None => {
                        report_send_error(&state, "Failed to send message: no active host".into());
                        return;
                    }
                },
            };
            if refuse_unholdable(&state, &host) {
                return;
            }

            composer.begin();
            spawn_local(async move {
                // The composer still holds the text through this await. It is
                // cleared only inside `settle_submission`, and only once the
                // record has taken custody.
                let (target, outcome) = match active_target {
                    Some((active, stream)) => {
                        let payload = protocol::SendMessagePayload {
                            message: text.clone(),
                            images: None,
                            origin: None,
                            tool_response: None,
                        };
                        let outcome = crate::send::send_frame(
                            &active.local_host_id,
                            stream,
                            protocol::FrameKind::SendMessage,
                            &payload,
                        )
                        .await;
                        (SubmissionTarget::Agent(active.as_agent_ref()), outcome)
                    }
                    None => {
                        // A new chat has no agent yet, and the client must not
                        // guess which `NewAgent` is its own — so this record is
                        // host-scoped, not attached to any agent.
                        let outcome =
                            crate::actions::spawn_new_chat(&state, text.clone(), vec![]).await;
                        (SubmissionTarget::NewChat, outcome)
                    }
                };
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target,
                        text,
                        images: Vec::new(),
                    },
                    outcome,
                );
            });
        }
    };

    let send_for_key = do_send.clone();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            send_for_key();
        }
    };

    let do_steer = {
        let state = state.clone();
        move || {
            if composer.is_busy() {
                return;
            }
            let Some(active) = state.active_agent.get_untracked() else {
                return;
            };
            let Some(stream) = active_agent_stream(&state, &active) else {
                report_send_error(&state, "Failed to steer: agent stream not found".into());
                return;
            };

            let text = state.chat_input.get_untracked().trim().to_string();
            let host = active.local_host_id.clone();
            if !text.is_empty() && refuse_unholdable(&state, &host) {
                return;
            }

            let state = state.clone();
            composer.begin();
            spawn_local(async move {
                // The composer keeps the draft across both sends. The interrupt
                // carries no user text, so only the message that follows it
                // becomes a recovery record.
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream.clone(),
                    protocol::FrameKind::Interrupt,
                    &protocol::InterruptPayload {},
                )
                .await
                {
                    composer.finish();
                    report_send_error(&state, format!("Failed to interrupt current turn: {error}"));
                    return;
                }
                if text.is_empty() {
                    composer.finish();
                    return;
                }
                let payload = protocol::SendMessagePayload {
                    message: text.clone(),
                    images: None,
                    origin: None,
                    tool_response: None,
                };
                let outcome = crate::send::send_frame(
                    &active.local_host_id,
                    stream,
                    protocol::FrameKind::SendMessage,
                    &payload,
                )
                .await;
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target: SubmissionTarget::Agent(active.as_agent_ref()),
                        text,
                        images: Vec::new(),
                    },
                    outcome,
                );
            });
        }
    };

    let steer_for_menu = do_steer.clone();

    // Plain interrupt: stop the current turn without sending the draft. The
    // menu's "Interrupt" item can appear while a draft exists, so it needs a
    // handler distinct from steer (which interrupts *and* sends the draft).
    let do_interrupt = {
        let state = state.clone();
        move || {
            let Some(active) = state.active_agent.get_untracked() else {
                return;
            };
            let Some(stream) = active_agent_stream(&state, &active) else {
                report_send_error(&state, "Failed to interrupt: agent stream not found".into());
                return;
            };
            let state = state.clone();
            spawn_local(async move {
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream,
                    protocol::FrameKind::Interrupt,
                    &protocol::InterruptPayload {},
                )
                .await
                {
                    report_send_error(&state, format!("Failed to interrupt current turn: {error}"));
                }
            });
        }
    };
    let interrupt_for_menu = do_interrupt;

    // "Fork + send": fork the active agent's session and send the draft to the
    // fork. Enabled only when there is draft text and the active agent has a
    // forkable backend session. The fork is a *new* agent, so — like new chat —
    // its recovery record is host-scoped, never attributed to the agent we
    // forked from.
    let do_btw = {
        let state = state.clone();
        move || {
            if composer.is_busy() {
                return;
            }
            let text = state.chat_input.get_untracked().trim().to_string();
            if text.is_empty() {
                return;
            }
            let Some(host) = state
                .active_agent
                .get_untracked()
                .map(|active| active.local_host_id)
            else {
                report_send_error(
                    &state,
                    "Failed to start side question: no active agent".into(),
                );
                return;
            };
            if refuse_unholdable(&state, &host) {
                return;
            }
            let state = state.clone();
            composer.begin();
            spawn_local(async move {
                let outcome =
                    crate::actions::spawn_side_question(&state, text.clone(), vec![]).await;
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target: SubmissionTarget::NewChat,
                        text,
                        images: Vec::new(),
                    },
                    outcome,
                );
            });
        }
    };
    let btw_for_menu = do_btw.clone();
    let send_for_menu = do_send.clone();

    let s_input = state.clone();
    let textarea_ref_for_effect = textarea_ref;
    Effect::new(move |_| {
        let _ = s_input.chat_input.get();
        if let Some(textarea) = textarea_ref_for_effect.get() {
            resize_chat_input(&textarea);
        }
    });

    let s_input = state.clone();
    let running_state = state.clone();
    let is_running = Memo::new(move |_| active_agent_is_running_tracked(&running_state));
    let has_text_state = state.clone();
    let has_text = Memo::new(move |_| has_text_state.chat_input.with(|t| !t.trim().is_empty()));
    let btw_state = state.clone();
    let can_btw = Memo::new(move |_| {
        btw_state.chat_input.with(|t| !t.trim().is_empty())
            && active_agent_has_session_id_tracked(&btw_state)
    });
    // Steer = thinking + draft typed.
    let is_steer = Memo::new(move |_| is_running.get() && has_text.get());
    // Menu holds items only for: Fork + send (input+session) or Steer+Cancel (thinking+input).
    // Steer and Fork + send are submissions too, and both spend money (Fork + send
    // creates an agent). The in-flight latch closes the whole surface, not just
    // the primary button — otherwise the dropdown is a way around the guard.
    let menu_has_items = Memo::new(move |_| (can_btw.get() || is_steer.get()) && !submitting.get());
    let menu_open = RwSignal::new(false);
    // Auto-dismiss a stale-open menu when its items disappear.
    Effect::new(move |_| {
        if !menu_has_items.get() {
            menu_open.set(false);
        }
    });
    let on_split_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" && menu_open.get() {
            ev.prevent_default();
            menu_open.set(false);
        }
    };

    let queue_state = state.clone();
    let queued_rows = Memo::new(move |_| {
        let Some(active) = queue_state.active_agent.get() else {
            return Vec::new();
        };
        let agent_ref = active.as_agent_ref();
        queue_state.agent_message_queue.with(|queues| {
            queues
                .get(&agent_ref)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| QueuedRowRef {
                            agent_ref: agent_ref.clone(),
                            id: entry.id.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    });

    view! {
        <div class="chat-input-container" data-mobile-test="chat-input-container">
            // The composer emptying itself is visible feedback for a sighted
            // user and silence for everyone else. This announces the move — and
            // says "queued", never "sent", because that is all the client knows.
            <div
                role="status"
                aria-live="polite"
                style=VISUALLY_HIDDEN
                data-mobile-test="chat-input-announcement"
            >
                {move || composer.announcement.get()}
            </div>
            {move || {
                let rows = queued_rows.get();
                if rows.is_empty() {
                    return view! { <div></div> }.into_any();
                }
                let n = rows.len();
                view! {
                    <div class="chat-input-queued-list" data-mobile-test="chat-input-queued-list" aria-live="polite">
                        <div class="chat-input-queued-title">
                            {format!("{n} message{} queued", if n == 1 { "" } else { "s" })}
                        </div>
                        <For
                            each=move || queued_rows.get()
                            key=|row| format!("{}:{}:{}", row.agent_ref.local_host_id, row.agent_ref.agent_id, row.id)
                            let:row
                        >
                            <QueuedMessageControlRow row=row />
                        </For>
                    </div>
                }.into_any()
            }}
            <div class="chat-input-row">
                <textarea
                    class="chat-input-field"
                    placeholder="Message..."
                    aria-label="Message composer"
                    rows=1
                    data-mobile-test="chat-input"
                    node_ref=textarea_ref
                    prop:value=move || s_input.chat_input.get()
                    on:input=move |ev| {
                        let textarea = event_target::<web_sys::HtmlTextAreaElement>(&ev);
                        let val = textarea.value();
                        s_input.chat_input.set(val);
                        resize_chat_input(&textarea);
                    }
                    on:keydown=on_keydown
                />
                <div
                    class="chat-send-split"
                    role="group"
                    aria-label="Send actions"
                    data-mobile-test="chat-send-split"
                    on:keydown=on_split_keydown
                >
                    <button
                        type="button"
                        class="send-button chat-send-split-primary"
                        aria-label={move || {
                            if is_running.get() && !has_text.get() { "Cancel current turn" }
                            else if is_steer.get() { "Queue message" }
                            else { "Send message" }
                        }}
                        data-mobile-test="chat-send"
                        on:click={
                            let do_interrupt = interrupt_for_menu.clone();
                            let do_send = send_for_menu.clone();
                            move |_| {
                                if is_running.get_untracked() && !has_text.get_untracked() {
                                    do_interrupt();
                                } else {
                                    do_send();
                                }
                            }
                        }
                        disabled=move || {
                            // Cancel (thinking+empty): always enabled — stopping a
                            // turn must never be blocked by an unsettled send.
                            if is_running.get() && !has_text.get() { false }
                            // The composer now keeps its text across the in-flight
                            // window, so "has text" no longer implies "not already
                            // sending". Say so explicitly.
                            else { !has_text.get() || submitting.get() }
                        }
                    >
                        {move || {
                            if is_running.get() && !has_text.get() { "Cancel" }
                            else if is_steer.get() { "Queue" }
                            else { "Send" }
                        }}
                    </button>
                    <button
                        type="button"
                        class="send-menu-toggle"
                        data-mobile-test="chat-send-menu-toggle"
                        aria-haspopup="menu"
                        aria-expanded=move || {
                            if menu_open.get() { "true" } else { "false" }
                        }
                        aria-label="More send actions"
                        disabled=move || !menu_has_items.get()
                        on:click=move |_| menu_open.update(|open| *open = !*open)
                    >
                        <span aria-hidden="true">"\u{2304}"</span>
                    </button>
                    {move || {
                        if !(menu_open.get() && menu_has_items.get()) {
                            return view! { <div></div> }.into_any();
                        }
                        let on_btw = btw_for_menu.clone();
                        let on_steer = steer_for_menu.clone();
                        let on_cancel = interrupt_for_menu.clone();
                        let show_steer = is_steer.get();
                        let show_btw = can_btw.get();
                        let show_cancel = is_steer.get();
                        view! {
                            <div
                                class="chat-send-menu-backdrop"
                                data-mobile-test="chat-send-menu-backdrop"
                                on:click=move |_| menu_open.set(false)
                            ></div>
                            <div
                                class="chat-send-menu"
                                role="menu"
                                aria-label="Send actions"
                                data-mobile-test="chat-send-menu"
                            >
                                {show_steer.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-steer"
                                        on:click=move |_| { menu_open.set(false); on_steer(); }
                                    >
                                        "Steer"
                                    </button>
                                })}
                                {show_btw.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-ask-aside"
                                        on:click=move |_| { menu_open.set(false); on_btw(); }
                                    >
                                        "Fork + send"
                                    </button>
                                })}
                                {show_cancel.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-cancel"
                                        on:click=move |_| { menu_open.set(false); on_cancel(); }
                                    >
                                        "Cancel"
                                    </button>
                                })}
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
}

fn resize_chat_input(textarea: &web_sys::HtmlTextAreaElement) {
    let html_el: web_sys::HtmlElement = textarea.clone().unchecked_into();
    let _ = textarea.set_attribute("style", "height: auto; overflow-y: hidden;");
    let scroll_height = html_el.scroll_height();
    let target_height = scroll_height.clamp(CHAT_INPUT_MIN_HEIGHT_PX, CHAT_INPUT_MAX_HEIGHT_PX);
    let overflow = if scroll_height > CHAT_INPUT_MAX_HEIGHT_PX {
        "auto"
    } else {
        "hidden"
    };
    let _ = textarea.set_attribute(
        "style",
        &format!("height: {target_height}px; overflow-y: {overflow};"),
    );
}

fn report_send_error(state: &AppState, message: String) {
    log::error!("{message}");
    state
        .mobile_shell_error
        .set(Some(crate::state::MobileShellError {
            code: protocol::MobileAccessErrorCode::TransportFailed,
            message,
        }));
}

/// Settle one outbound user submission.
///
/// **On admission the text and images move into a recovery record and the
/// composer is cleared in the same synchronous step.** There is never a state in
/// which the user's input exists in neither place. That window was the bug: the
/// composer used to be cleared *before* the send was awaited, so a send that
/// never settled destroyed the text silently — no holder, no error, nothing.
///
/// **On rejection nothing moves.** The text never left the composer, so there is
/// nothing to restore; the user just sees why it did not go.
///
/// Admission means the frame entered this connection's bounded outbound queue.
/// It is not delivery, and this function never claims otherwise: the record is
/// born silent and is only ever surfaced if the transport later reports a
/// failure for it.
fn settle_submission(
    state: &AppState,
    composer: Composer,
    submission: OutboundSubmission,
    outcome: Result<Accepted, SendFrameError>,
) {
    let OutboundSubmission {
        local_host_id,
        target,
        text,
        images,
    } = submission;
    match outcome {
        Ok(accepted) => {
            state.hold_submission(PendingSubmission {
                local_submission_id: accepted.local_submission_id,
                // A fresh logical submission: the user just made this one.
                origin: state.mint_submission_origin(),
                local_host_id,
                connection_instance_id: accepted.connection_instance_id,
                target,
                text,
                images,
                // The composer only ever produces plain chat messages. Typed tool
                // responses (a plan approval, a rejection) are submitted by their
                // card, never typed here.
                tool_response: None,
                state: PendingSubmissionState::QueuedLocally,
            });
            composer.clear(state);
            composer.announcement.set(QUEUED_ANNOUNCEMENT.to_owned());
        }
        Err(error) => {
            // The composer still holds the text — nothing moved, nothing to
            // restore. Say exactly why it did not go.
            composer.announcement.set(String::new());
            report_send_error(state, error.to_string());
        }
    }
    composer.finish();
}

/// Refuse a send whose text we could not take custody of, **before** the frame
/// reaches the transport.
///
/// This is the only honest place to enforce the cap. Once a frame is admitted it
/// cannot be un-sent, so a cap enforced afterwards can only make room by
/// destroying a record — and every record, in-flight ones included, is the sole
/// holder of a message the user may still need back.
///
/// So the send simply does not happen, the composer keeps every character, and
/// the user is told why.
fn refuse_unholdable(state: &AppState, host: &LocalHostId) -> bool {
    if state.can_hold_submission_untracked(host) {
        return false;
    }
    report_send_error(
        state,
        "Not sent — too many messages on this host are still unresolved. Deal with the ones \
         waiting below, then send this again. Your text is still here."
            .to_owned(),
    );
    true
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::bridge::{LocalSubmissionId, SendRejected};
    use crate::state::{AgentInfo, AgentRef, AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, QueuedMessageEntry, QueuedMessageId, SessionId,
        StreamPath,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn accepted(id: u64) -> Accepted {
        Accepted {
            connection_instance_id: 7,
            local_submission_id: LocalSubmissionId(id),
        }
    }

    /// The regression test for the bug this whole model exists to kill.
    ///
    /// The composer used to be cleared *before* the send was awaited. A send
    /// that never settled therefore destroyed the user's text outright: it was
    /// gone from the composer, it was in no record, and no error ever ran. The
    /// message simply vanished.
    ///
    /// The contract now: on admission the text leaves the composer **and**
    /// lands in a record. Both, together. There is no observable state in which
    /// it exists in neither place.
    #[wasm_bindgen_test]
    async fn admission_moves_text_out_of_composer_and_into_a_record() {
        let state = AppState::new();
        let host = LocalHostId("host-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.chat_input.set("ship it".to_owned());

        let composer = Composer::new();

        settle_submission(
            &state,
            composer,
            OutboundSubmission {
                local_host_id: host.clone(),
                target: SubmissionTarget::NewChat,
                text: "ship it".to_owned(),
                images: Vec::new(),
            },
            Ok(accepted(1)),
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "composer must be cleared once the text has a holder"
        );
        let held = state
            .pending_submissions
            .get_untracked()
            .get(&LocalSubmissionId(1))
            .cloned()
            .expect("the admitted text must be held in a recovery record");
        assert_eq!(
            held.text, "ship it",
            "the record must hold the exact text that left the composer"
        );
        assert_eq!(
            held.state,
            PendingSubmissionState::QueuedLocally,
            "an admitted submission is queued locally — never 'sent'"
        );
        assert_eq!(
            held.target,
            SubmissionTarget::NewChat,
            "a new chat has no agent, so the record must not claim one"
        );
        assert_eq!(
            composer.announcement.get_untracked(),
            QUEUED_ANNOUNCEMENT,
            "the move must be announced, so it is not silent for a screen reader"
        );
        assert!(
            !composer.is_busy(),
            "settling must release the in-flight latch, or the composer stays wedged"
        );
    }

    /// A rejected submission never left the composer, so there is nothing to
    /// restore — and nothing to hold. The user sees their text still sitting
    /// there, plus the exact reason it did not go.
    #[wasm_bindgen_test]
    async fn rejection_leaves_the_composer_untouched_and_holds_nothing() {
        let state = AppState::new();
        let host = LocalHostId("host-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.chat_input.set("ship it".to_owned());

        let composer = Composer::new();

        settle_submission(
            &state,
            composer,
            OutboundSubmission {
                local_host_id: host,
                target: SubmissionTarget::NewChat,
                text: "ship it".to_owned(),
                images: Vec::new(),
            },
            Err(SendFrameError::Rejected(SendRejected::NotConnected)),
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "ship it",
            "a rejected send must leave the user's text exactly where it was"
        );
        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "a rejected send was never admitted, so nothing may be held for it"
        );
        let surfaced = state
            .mobile_shell_error
            .get_untracked()
            .expect("the admission failure must be surfaced, not swallowed");
        assert!(
            surfaced
                .message
                .contains(&SendRejected::NotConnected.to_string()),
            "the exact admission error must be shown, got: {}",
            surfaced.message
        );
    }

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

    /// Click the split-button caret to reveal the action menu.
    async fn open_menu(container: &HtmlElement) {
        let toggle: HtmlElement = container
            .query_selector("[data-mobile-test='chat-send-menu-toggle']")
            .unwrap()
            .expect("dropdown toggle must be present")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;
    }

    fn primary(container: &HtmlElement) -> web_sys::Element {
        container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .expect("primary button must be present")
    }

    fn caret(container: &HtmlElement) -> web_sys::Element {
        container
            .query_selector("[data-mobile-test='chat-send-menu-toggle']")
            .unwrap()
            .expect("caret button must always be present")
    }

    fn menu_item_texts(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all("[role='menuitem']").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|n| n.text_content().unwrap_or_default().trim().to_owned())
            .collect()
    }

    fn type_text(container: &HtmlElement, text: &str) {
        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value(text);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    /// Mount a composer in new-chat mode (no active agent) on a connected host.
    fn mount_new_chat(container: &HtmlElement) -> AppState {
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId("host-1".to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.host_streams.update(|m| {
                m.insert(host, StreamPath("/host/h1".to_owned()));
            });
            // The backend override is all `spawn_new_chat` needs to pick a
            // backend, so the test does not have to fabricate host settings.
            state.draft_backend_override.set(Some(BackendKind::Claude));
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatInput /> }
        });
        std::mem::forget(h);
        handle.borrow().as_ref().unwrap().clone()
    }

    /// **A double-tap must never buy two agents.**
    ///
    /// The old code cleared the composer *before* awaiting the send, so a second
    /// tap read an empty box and fell out of `if text.is_empty()`. That was never
    /// a guard — it was a side effect of the very bug being fixed. Preserving the
    /// user's text across the in-flight window removed it, and nothing replaced
    /// it: `Send` stayed enabled the whole time.
    ///
    /// It did not misfire only because `send_line` happened to resolve in the
    /// same microtask drain, so the future finished before the next click could
    /// be dispatched. That is an accident of executor ordering, not a guarantee.
    /// Any future `await` on that chain that genuinely yields — backpressure, an
    /// IndexedDB read, an auth refresh — reopens it, and two `SpawnAgent` frames
    /// is two agents, two backend sessions, and two paid turns.
    ///
    /// So this test makes the send **actually yield** (the deferred seam awaits a
    /// oneshot) and then taps twice. Exactly one frame must go out.
    #[wasm_bindgen_test]
    async fn a_double_tap_during_an_unsettled_send_emits_exactly_one_frame() {
        let _guard = crate::bridge::test_defer_sends();
        let container = make_container();
        let state = mount_new_chat(&container);
        next_tick().await;

        type_text(&container, "start a new chat");
        next_tick().await;

        let send: HtmlElement = primary(&container).dyn_into().unwrap();
        assert!(!send.has_attribute("disabled"), "Send must start enabled");

        // First tap. The send is deferred, so it is genuinely unsettled: the
        // future is parked on the oneshot and the composer still holds the text.
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "the first tap must send exactly one frame"
        );
        assert_eq!(
            state.chat_input.get_untracked(),
            "start a new chat",
            "the composer must still hold the text while the send is unsettled — \
             that is the whole point, and it is what removed the accidental guard"
        );
        assert!(
            send.has_attribute("disabled"),
            "Send must be disabled while a submission is unsettled"
        );

        // The impatient second tap, while the first is still in flight.
        send.click();
        next_tick().await;
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "a double-tap during an unsettled send must not buy a second agent"
        );

        // Let the first one land; the composer empties and reopens for the next.
        crate::bridge::test_resolve_next_send();
        next_tick().await;
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "once admitted, the text moves to the record and the composer clears"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "exactly one submission must be held"
        );
    }

    /// The same guard, on the other frame kind. `SendMessage` to a live agent does
    /// not create an agent, but a double-tap still delivers the user's message
    /// twice — and a duplicate turn is a duplicate paid turn.
    #[wasm_bindgen_test]
    async fn a_double_tap_on_an_agent_message_emits_exactly_one_frame() {
        let _guard = crate::bridge::test_defer_sends();
        let container = make_container();
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![AgentInfo {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "run the tests");
        next_tick().await;

        let send: HtmlElement = primary(&container).dyn_into().unwrap();
        send.click();
        next_tick().await;
        assert_eq!(crate::bridge::test_send_attempts(), 1);
        assert!(
            send.has_attribute("disabled"),
            "Send must be disabled while the message is unsettled"
        );

        send.click();
        next_tick().await;
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "an impatient double-tap must not send the message twice"
        );
    }

    // ── State matrix row 1: Idle + empty ─────────────────────────────────────
    // Primary "Send" disabled; caret visible but disabled.
    #[wasm_bindgen_test]
    async fn idle_empty_send_disabled_caret_disabled() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            p.has_attribute("disabled"),
            "Send must be disabled when empty"
        );

        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled with no menu items"
        );
    }

    /// When there are queued messages, the composer surfaces per-row
    /// controls so a phone can do the same send-now/delete operations as
    /// desktop — without disabling the input.
    #[wasm_bindgen_test]
    async fn queued_controls_appear_when_messages_are_queued() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_message_queue.update(|m| {
                m.insert(
                    agent_ref,
                    vec![QueuedMessageEntry {
                        id: QueuedMessageId("q-1".to_owned()),
                        message: "later".to_owned(),
                        images: Vec::new(),
                        origin: None,
                    }],
                );
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;
        let list = container
            .query_selector("[data-mobile-test='chat-input-queued-list']")
            .unwrap()
            .expect("queued controls must render when at least one message is queued");
        let text = list.text_content().unwrap_or_default();
        assert!(
            text.contains("1 message"),
            "queued controls must mention count: {text}"
        );
        assert!(
            list.query_selector("[data-mobile-test='chat-input-queued-send-now']")
                .unwrap()
                .is_some(),
            "queued row must expose Send Now"
        );
        assert!(
            list.query_selector("[data-mobile-test='chat-input-queued-delete']")
                .unwrap()
                .is_some(),
            "queued row must expose Delete"
        );
        // Composer must remain enabled for queueing more messages.
        let input = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap();
        assert!(
            !input.has_attribute("disabled"),
            "composer must stay enabled so users can queue more"
        );
    }

    // ── State matrix row 4: Thinking + empty ─────────────────────────────────
    // Primary "Cancel" enabled; caret disabled; no menu items.
    #[wasm_bindgen_test]
    async fn thinking_empty_primary_cancel_caret_disabled() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Cancel",
            "primary must be Cancel when thinking with empty composer"
        );
        assert!(
            !p.has_attribute("disabled"),
            "Cancel must be enabled while thinking"
        );

        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled when thinking+empty (no menu items)"
        );
    }

    // ── State matrix row 5: Thinking + input, no session ─────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_no_session_queue_primary_steer_cancel_menu() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "redirect this");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Queue",
            "primary must be Queue when thinking with draft"
        );
        assert!(!p.has_attribute("disabled"), "Queue must be enabled");

        assert!(
            container
                .query_selector("[data-mobile-test='chat-steer']")
                .unwrap()
                .is_none(),
            "no standalone Steer button — it lives in the dropdown"
        );

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec!["Steer".to_owned(), "Cancel".to_owned()],
            "thinking+input menu must be Steer then Cancel"
        );
    }

    // ── State matrix row 3: Idle + input + session ───────────────────────────
    // Primary "Send" enabled; caret enabled; dropdown has "Fork + send" only.
    #[wasm_bindgen_test]
    async fn idle_input_with_session_menu_fork_send_only() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        // No draft → caret present but disabled.
        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled while no menu items (idle, no draft)"
        );

        type_text(&container, "why is this slow?");
        next_tick().await;

        // Now has draft → caret enabled, menu has "Fork + send" only.
        let c = caret(&container);
        assert!(
            !c.has_attribute("disabled"),
            "caret must be enabled once draft + session"
        );

        open_menu(&container).await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-ask-aside']")
                .unwrap()
                .is_some(),
            "Fork + send must appear once there is draft text and a forkable session"
        );
        assert_eq!(
            menu_item_texts(&container),
            vec!["Fork + send".to_owned()],
            "idle+session menu must be exactly 'Fork + send'"
        );
        // Fork + send must only exist inside the dropdown, not as a standalone button.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
                .unwrap()
                .is_none(),
            "Fork + send must only exist inside the dropdown menu"
        );
    }

    // ── State matrix row 2: Idle + input, no session ─────────────────────────
    // Primary "Send" enabled; caret disabled (no menu items).
    #[wasm_bindgen_test]
    async fn idle_input_no_session_send_enabled_caret_disabled() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "anything");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            !p.has_attribute("disabled"),
            "Send must be enabled with draft"
        );

        // No session → Fork + send absent → caret disabled.
        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled with no session (idle+input)"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-ask-aside']")
                .unwrap()
                .is_none(),
            "Fork + send must stay hidden when the active agent has no session id"
        );
    }

    // ── State matrix row 6: Thinking + input + session ───────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Fork + send", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_with_session_queue_primary_full_menu() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "redirect this");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Queue",
            "primary must be Queue when thinking with draft"
        );
        assert!(!p.has_attribute("disabled"), "Queue must be enabled");

        let c = caret(&container);
        assert!(!c.has_attribute("disabled"), "caret must be enabled");

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec![
                "Steer".to_owned(),
                "Fork + send".to_owned(),
                "Cancel".to_owned(),
            ],
            "thinking+session+input menu must be Steer, Fork + send, Cancel"
        );
    }

    /// Multiline input should grow vertically instead of hiding all but
    /// one or two lines. The resize helper caps growth and then scrolls
    /// internally for very long drafts.
    #[wasm_bindgen_test]
    async fn composer_resizes_for_multiline_input() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("one\ntwo\nthree\nfour\nfive\nsix");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        let style = input.get_attribute("style").unwrap_or_default();
        assert!(
            style.contains("height:") && style.contains("overflow-y:"),
            "composer should get an inline autosize style, got: {style}"
        );
    }
}
