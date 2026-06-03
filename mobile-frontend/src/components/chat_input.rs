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
/// required to fork a BTW / side question off it.
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
/// Sends on tap of the primary "Send" button or Enter (without Shift). Stays
/// enabled while a turn is active so the user can queue follow-up messages,
/// and surfaces queued-message controls above the composer. A caret next to
/// Send opens a dropdown of the available actions (Send, BTW, Interrupt and
/// send now, Interrupt) whenever any of them apply. Send is disabled only when
/// the composer is empty, the standard mobile pattern users already expect.
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

    let send_for_click = do_send.clone();
    let on_send_click = move |_| send_for_click();

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

    // BTW / side question: fork the active agent's session into a fresh
    // read-only agent seeded with the current draft, then clear the draft
    // optimistically (mirroring send). Enabled only when there is draft text
    // and the active agent has a forkable backend session.
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
    // Split-button menu state. `is_steer` (running with a draft) gates the
    // extra "Interrupt and send now" item. The caret is shown whenever the
    // menu would hold at least one item.
    let is_steer = Memo::new(move |_| is_running.get() && has_text.get());
    let menu_has_items = Memo::new(move |_| has_text.get() || can_btw.get() || is_running.get());
    let menu_open = RwSignal::new(false);
    // Auto-dismiss a stale-open menu when its items disappear (turn ends or the
    // draft is cleared) so it can't silently re-appear when they return.
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
                        aria-label="Send message"
                        data-mobile-test="chat-send"
                        on:click=on_send_click
                        disabled=move || !has_text.get()
                    >
                        "Send"
                    </button>
                    {move || {
                        // Hide the caret entirely when the menu would be empty,
                        // so the composer row stays tight on narrow screens.
                        if !menu_has_items.get() {
                            return view! { <div></div> }.into_any();
                        }
                        view! {
                            <button
                                type="button"
                                class="send-menu-toggle"
                                data-mobile-test="chat-send-menu-toggle"
                                aria-haspopup="menu"
                                aria-expanded=move || {
                                    if menu_open.get() { "true" } else { "false" }
                                }
                                aria-label="More send actions"
                                on:click=move |_| menu_open.update(|open| *open = !*open)
                            >
                                <span aria-hidden="true">"\u{2304}"</span>
                            </button>
                        }.into_any()
                    }}
                    {move || {
                        if !(menu_open.get() && menu_has_items.get()) {
                            return view! { <div></div> }.into_any();
                        }
                        let on_send = send_for_menu.clone();
                        let on_btw = btw_for_menu.clone();
                        let on_steer = steer_for_menu.clone();
                        let on_interrupt = interrupt_for_menu.clone();
                        let show_send = has_text.get();
                        let show_btw = can_btw.get();
                        let show_steer = is_steer.get();
                        let show_interrupt = is_running.get();
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
                                {show_send.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-send"
                                        on:click=move |_| { menu_open.set(false); on_send(); }
                                    >
                                        "Send"
                                    </button>
                                })}
                                {show_btw.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-btw"
                                        on:click=move |_| { menu_open.set(false); on_btw(); }
                                    >
                                        "BTW"
                                    </button>
                                })}
                                {show_steer.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-steer"
                                        on:click=move |_| { menu_open.set(false); on_steer(); }
                                    >
                                        "Interrupt and send now"
                                    </button>
                                })}
                                {show_interrupt.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-interrupt"
                                        on:click=move |_| { menu_open.set(false); on_interrupt(); }
                                    >
                                        "Interrupt"
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

    /// Visible text of each menu item, in DOM order.
    fn menu_item_texts(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all("[role='menuitem']").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|n| n.text_content().unwrap_or_default().trim().to_owned())
            .collect()
    }

    /// Send button is disabled when the composer is empty and enables
    /// when text is typed. This is the touch-target affordance — the
    /// user must see the button "turn on" before they tap.
    #[wasm_bindgen_test]
    async fn send_button_disabled_when_input_empty_and_enables_on_input() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;
        let btn = container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .unwrap();
        assert!(
            btn.has_attribute("disabled"),
            "send must start disabled when input is empty"
        );

        // Type something and verify the disabled attribute clears.
        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("hello");
        let ev = web_sys::Event::new("input").unwrap();
        input.dispatch_event(&ev).unwrap();
        next_tick().await;
        let btn = container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .unwrap();
        assert!(
            !btn.has_attribute("disabled"),
            "send must enable after typing non-whitespace"
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

    /// While a turn is active with typed input, the primary stays "Send"
    /// (never Queue/Steer) and the dropdown surfaces the interrupt actions.
    /// The old standalone Steer button must be gone.
    #[wasm_bindgen_test]
    async fn running_turn_menu_offers_interrupt_actions() {
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

        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("redirect this");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        let send = container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .expect("send button");
        assert_eq!(
            send.text_content().unwrap_or_default().trim(),
            "Send",
            "primary label must always be Send, never Queue"
        );
        // Steering now lives in the dropdown, not a standalone button.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-steer']")
                .unwrap()
                .is_none(),
            "the old standalone Steer button must no longer exist"
        );

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec![
                "Send".to_owned(),
                "Interrupt and send now".to_owned(),
                "Interrupt".to_owned(),
            ],
            "running+input menu must offer Send and both interrupt actions"
        );
    }

    /// The BTW (side question) menu item only appears once there's draft text
    /// AND the active agent has a forkable backend session. With no draft the
    /// caret itself is hidden (empty menu); once text is typed the menu exposes
    /// BTW alongside Send.
    #[wasm_bindgen_test]
    async fn btw_menu_item_requires_session_and_text() {
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

        // No draft text and an idle agent → empty menu, so the caret itself
        // (and therefore BTW) is hidden.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-toggle']")
                .unwrap()
                .is_none(),
            "dropdown caret must stay hidden while the menu would be empty"
        );

        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("why is this slow?");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        open_menu(&container).await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-btw']")
                .unwrap()
                .is_some(),
            "BTW menu item must appear once there is draft text and a forkable session"
        );
        // It must never be a standalone composer button anymore.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
                .unwrap()
                .is_none(),
            "BTW must only exist inside the dropdown menu, not as a standalone button"
        );
    }

    /// Without a backend session id on the active agent, the BTW menu item
    /// stays hidden no matter what's typed — there's nothing to fork — even
    /// though the dropdown still opens to offer Send.
    #[wasm_bindgen_test]
    async fn btw_menu_item_hidden_without_session() {
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

        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("anything");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        // The caret shows (Send is available) but the menu omits BTW.
        open_menu(&container).await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-send']")
                .unwrap()
                .is_some(),
            "menu must still offer Send"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-btw']")
                .unwrap()
                .is_none(),
            "BTW must stay hidden when the active agent has no session id"
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
