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
/// Sends on tap of the up-arrow button or Enter (without Shift). Stays
/// enabled while a turn is active so the user can queue follow-up
/// messages, surfaces queued-message controls above the composer, and
/// exposes a Steer action while the agent is running. The send button is
/// the only thing disabled when the input is empty, which is the standard
/// mobile pattern users already expect.
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

    let steer_for_view = do_steer.clone();

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
    let btw_for_view = do_btw.clone();

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
                <button
                    type="button"
                    class="send-button"
                    aria-label=move || {
                        if is_running.get() {
                            "Queue message"
                        } else {
                            "Send message"
                        }
                    }
                    data-mobile-test="chat-send"
                    on:click=on_send_click
                    disabled=move || {
                        !has_text.get()
                    }
                >
                    {move || if is_running.get() { "Queue" } else { "\u{2191}" }}
                </button>
                {move || {
                    // Only render when forkable — keeps the row tight on
                    // narrow screens instead of showing a dead disabled button.
                    if !can_btw.get() {
                        return view! { <div></div> }.into_any();
                    }
                    let on_btw = btw_for_view.clone();
                    view! {
                        <button
                            type="button"
                            class="btw-button"
                            aria-label="Ask a side question (BTW) — forks this session read-only"
                            data-mobile-test="chat-btw"
                            on:click=move |_| on_btw()
                        >
                            "BTW"
                        </button>
                    }.into_any()
                }}
                {move || {
                    if !is_running.get() {
                        return view! { <div></div> }.into_any();
                    }
                    let on_steer = steer_for_view.clone();
                    view! {
                        <button
                            type="button"
                            class="steer-button"
                            aria-label=move || {
                                if has_text.get() {
                                    "Interrupt and send typed message now"
                                } else {
                                    "Stop current turn"
                                }
                            }
                            data-mobile-test="chat-steer"
                            on:click=move |_| on_steer()
                        >
                            {move || if has_text.get() { "Steer" } else { "Stop" }}
                        </button>
                    }.into_any()
                }}
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

    /// While a turn is active, typed input can either be queued (normal
    /// send) or used to steer by interrupting the current turn first.
    #[wasm_bindgen_test]
    async fn running_turn_shows_queue_and_steer_controls() {
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
        assert!(
            send.text_content().unwrap_or_default().contains("Queue"),
            "send button must visibly switch to Queue while the agent runs"
        );
        let steer = container
            .query_selector("[data-mobile-test='chat-steer']")
            .unwrap()
            .expect("steer button");
        assert!(
            steer.text_content().unwrap_or_default().contains("Steer"),
            "typed running-turn control must visibly offer Steer"
        );
    }

    /// The BTW (side question) affordance only appears once there's draft
    /// text AND the active agent has a forkable backend session — otherwise
    /// a fork can't be addressed, so a dead button would just waste the tight
    /// mobile row.
    #[wasm_bindgen_test]
    async fn btw_button_requires_session_and_text() {
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

        // No draft text yet → BTW hidden even with a session.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
                .unwrap()
                .is_none(),
            "BTW must stay hidden while the composer is empty"
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

        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
                .unwrap()
                .is_some(),
            "BTW must appear once there is draft text and a forkable session"
        );
    }

    /// Without a backend session id on the active agent, the BTW affordance
    /// stays hidden no matter what's typed — there's nothing to fork.
    #[wasm_bindgen_test]
    async fn btw_button_hidden_without_session() {
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

        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
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
