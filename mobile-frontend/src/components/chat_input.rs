use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::state::{AgentRef, AppState};

const CHAT_INPUT_MIN_HEIGHT_PX: i32 = 36;
const CHAT_INPUT_MAX_HEIGHT_PX: i32 = 132;

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueuedRowRef {
    agent_ref: AgentRef,
    id: protocol::QueuedMessageId,
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
    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();

    let do_send = {
        let state = state.clone();
        move || {
            let text = state.chat_input.get_untracked().trim().to_string();
            if text.is_empty() {
                return;
            }

            let state = state.clone();
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

            // Starting a genuinely-new turn ends the restore/replay phase:
            // freeze the history window so the last restored message stays
            // visible and this new exchange accumulates on screen instead of
            // being swallowed by the windowing tail-tracking.
            if let Some((active, _)) = active_target.as_ref() {
                let agent_ref = active.as_agent_ref();
                state.history_settling.update(|set| {
                    set.remove(&agent_ref);
                });
            }

            state.chat_input.set(String::new());
            if let Some(textarea) = textarea_ref.get_untracked() {
                textarea.set_value("");
                resize_chat_input(&textarea);
            }

            spawn_local(async move {
                if let Some((ar, stream)) = active_target {
                    let payload = protocol::SendMessagePayload {
                        message: text,
                        images: None,
                        origin: None,
                        tool_response: None,
                    };
                    if let Err(error) = crate::send::send_frame(
                        &ar.local_host_id,
                        stream,
                        protocol::FrameKind::SendMessage,
                        &payload,
                    )
                    .await
                    {
                        report_send_error(&state, format!("Failed to send message: {error}"));
                    }
                } else if let Err(e) = crate::actions::spawn_new_chat(&state, text, vec![]).await {
                    log::error!("failed to spawn chat: {e}");
                    report_send_error(&state, format!("Failed to start agent: {e}"));
                }
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
            let Some(active) = state.active_agent.get_untracked() else {
                return;
            };
            let Some(stream) = active_agent_stream(&state, &active) else {
                report_send_error(&state, "Failed to steer: agent stream not found".into());
                return;
            };

            let text = state.chat_input.get_untracked().trim().to_string();
            if !text.is_empty() {
                state.chat_input.set(String::new());
                if let Some(textarea) = textarea_ref.get_untracked() {
                    textarea.set_value("");
                    resize_chat_input(&textarea);
                }
            }

            let state = state.clone();
            spawn_local(async move {
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream.clone(),
                    protocol::FrameKind::Interrupt,
                    &protocol::InterruptPayload {},
                )
                .await
                {
                    report_send_error(&state, format!("Failed to interrupt current turn: {error}"));
                    return;
                }
                if text.is_empty() {
                    return;
                }
                let payload = protocol::SendMessagePayload {
                    message: text,
                    images: None,
                    origin: None,
                    tool_response: None,
                };
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream,
                    protocol::FrameKind::SendMessage,
                    &payload,
                )
                .await
                {
                    report_send_error(&state, format!("Failed to send steer message: {error}"));
                }
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

    // "Fork + send": fork the active agent's session and send the draft to the fork,
    // then clear the draft optimistically (mirroring send). Enabled only when
    // there is draft text and the active agent has a forkable backend session.
    let do_btw = {
        let state = state.clone();
        move || {
            let text = state.chat_input.get_untracked().trim().to_string();
            if text.is_empty() {
                return;
            }
            state.chat_input.set(String::new());
            if let Some(textarea) = textarea_ref.get_untracked() {
                textarea.set_value("");
                resize_chat_input(&textarea);
            }
            let state = state.clone();
            spawn_local(async move {
                if let Err(error) = crate::actions::spawn_side_question(&state, text, vec![]).await
                {
                    report_send_error(&state, format!("Failed to start side question: {error}"));
                }
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
    let menu_has_items = Memo::new(move |_| can_btw.get() || is_steer.get());
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
                            // Cancel (thinking+empty): always enabled.
                            if is_running.get() && !has_text.get() { false }
                            else { !has_text.get() }
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
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
