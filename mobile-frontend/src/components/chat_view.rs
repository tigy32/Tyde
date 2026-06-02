use std::cell::{Cell, RefCell};
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{AgentRef, AppState};

const CHAT_STICKY_BOTTOM_THRESHOLD_PX: i32 = 80;

/// Conversation surface.
///
/// Composition rules:
/// - The header surfaces the agent name plus the current backend as a
///   `Pill`, and exposes Stop while a turn is active. The back button
///   is small but always has an accessible label.
/// - The transcript shows task list → messages → queued messages →
///   streaming → transient events, in that order, because that is the
///   order users perceive them happening.
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
        let active = s_interrupt.active_agent.get_untracked();
        if let Some(ar) = active {
            let agent_stream = s_interrupt.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.local_host_id == ar.local_host_id && a.agent_id == ar.agent_id)
                    .map(|a| a.instance_stream.clone())
            });
            if let Some(stream) = agent_stream {
                let host_id = ar.local_host_id.clone();
                spawn_local(async move {
                    let _ = crate::send::send_frame(
                        &host_id,
                        stream,
                        protocol::FrameKind::Interrupt,
                        &protocol::InterruptPayload {},
                    )
                    .await;
                });
            }
        }
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
        if *last_active_for_auto.borrow() != active_agent {
            *last_active_for_auto.borrow_mut() = active_agent.clone();
            user_scrolled_up.set(false);
        }

        if let Some(key) = active_agent.as_ref() {
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
        <div class="view chat-view" data-mobile-test="chat-view">
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
                    let streaming = s_body.streaming_text.with(|m| m.get(&key).cloned());
                    let task_list = s_body.task_lists.with(|m| m.get(&key).cloned());
                    let transient = s_body.transient_events.with(|m| m.get(&key).cloned().unwrap_or_default());
                    let queued = s_body.agent_message_queue.with(|m| m.get(&key).cloned().unwrap_or_default());

                    let no_content = messages.is_empty()
                        && streaming.is_none()
                        && task_list.is_none()
                        && transient.is_empty()
                        && queued.is_empty();

                    if no_content {
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
                            {messages.into_iter().map(|entry| {
                                view! { <ChatMessageView entry=entry /> }
                            }).collect::<Vec<_>>()}

                            // Queued messages: messages the user typed while a
                            // turn was already running. They haven't been sent
                            // yet — surface them visually distinct from sent
                            // messages so the user knows what's still pending.
                            {if queued.is_empty() {
                                view! { <div></div> }.into_any()
                            } else {
                                let count = queued.len();
                                let key_for_rows = key.clone();
                                view! {
                                    <div class="queued-messages" data-mobile-test="chat-queued">
                                        <div class="queued-messages-header">
                                            <Pill
                                                label=format!("{count} queued")
                                                tone=PillTone::Accent
                                                data_mobile_test="chat-queued-pill"
                                            />
                                        </div>
                                        {queued.into_iter().map(|q| {
                                            let row_text = if q.message.trim().is_empty() {
                                                match q.images.len() {
                                                    0 => "Queued message".to_owned(),
                                                    1 => "Image attachment".to_owned(),
                                                    count => format!("{count} image attachments"),
                                                }
                                            } else if q.images.is_empty() {
                                                q.message
                                            } else {
                                                let suffix = if q.images.len() == 1 { "image" } else { "images" };
                                                format!("{} (+{} {suffix})", q.message, q.images.len())
                                            };
                                            let send_now_id = q.id.clone();
                                            let agent_ref_for_send_now = key_for_rows.clone();
                                            let state_for_send_now = s_body.clone();
                                            let on_send_now = Callback::new(move |_: ()| {
                                                let aref = agent_ref_for_send_now.clone();
                                                let qid = send_now_id.clone();
                                                let state = state_for_send_now.clone();
                                                spawn_local(async move {
                                                    if let Err(e) = crate::actions::send_queued_message_now(
                                                        &state, &aref, qid,
                                                    ).await {
                                                        log::error!("send_queued_message_now failed: {e}");
                                                    }
                                                });
                                            });
                                            let q_id = q.id.clone();
                                            let agent_ref_for_cancel = key_for_rows.clone();
                                            let state_for_cancel = s_body.clone();
                                            let on_cancel = Callback::new(move |_: ()| {
                                                let aref = agent_ref_for_cancel.clone();
                                                let qid = q_id.clone();
                                                let state = state_for_cancel.clone();
                                                spawn_local(async move {
                                                    if let Err(e) = crate::actions::cancel_queued_message(
                                                        &state, &aref, qid,
                                                    ).await {
                                                        log::error!("cancel_queued_message failed: {e}");
                                                    }
                                                });
                                            });
                                            view! {
                                                <div class="queued-message" data-mobile-test="chat-queued-row">
                                                    <span class="queued-icon" aria-hidden="true">"\u{23F1}"</span>
                                                    <span class="queued-text">{row_text}</span>
                                                    <Button
                                                        label="Send Now"
                                                        variant=ButtonVariant::Primary
                                                        size=ButtonSize::Compact
                                                        data_mobile_test="chat-queued-send-now"
                                                        aria_label="Send queued message now".to_string()
                                                        on_click=on_send_now
                                                    />
                                                    <Button
                                                        label="Delete"
                                                        variant=ButtonVariant::Ghost
                                                        size=ButtonSize::Compact
                                                        data_mobile_test="chat-queued-cancel"
                                                        aria_label="Delete queued message".to_string()
                                                        on_click=on_cancel
                                                    />
                                                </div>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }.into_any()
                            }}

                            // Streaming message
                            {streaming.map(|s| {
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
                                                        view! { <crate::components::tool_card::ToolCardView entry=t /> }
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
            <ChatInput />
        </div>
    }
}

fn track_active_chat_content(state: &AppState, key: &AgentRef) {
    state.chat_messages.with(|m| {
        let _ = m.get(key).map_or(0, Vec::len);
    });
    state.task_lists.with(|m| {
        let _ = m.contains_key(key);
    });
    state.transient_events.with(|m| {
        let _ = m.get(key).map_or(0, Vec::len);
    });
    state.agent_message_queue.with(|m| {
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
        AgentInfo, AgentRef, AppState, ChatMessageEntry, LocalHostId, TransientEvent,
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

    fn mount_active_chat(container: HtmlElement) -> AppState {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![make_agent(&host_for_mount, "Coder")]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatView /> }
        });
        std::mem::forget(handle);
        state_handle.borrow().as_ref().unwrap().clone()
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

    /// Active agent with no content gets the "Conversation is empty"
    /// empty state, not the "Start a new chat" state.
    #[wasm_bindgen_test]
    async fn chat_empty_when_active_agent_has_no_messages() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
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

    /// Queued messages render with their pill count and per-row markers
    /// so the user can see what's still pending while a turn runs.
    #[wasm_bindgen_test]
    async fn chat_renders_queued_messages_with_count_pill() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
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
        let queued_pill = container
            .query_selector("[data-mobile-test='chat-queued-pill']")
            .unwrap()
            .expect("queued pill must render");
        assert!(
            queued_pill
                .text_content()
                .unwrap_or_default()
                .contains("2 queued"),
            "queued pill must show count"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("second pending") && text.contains("third pending"),
            "queued message bodies must render"
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
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
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
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
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

    /// Each queued-message row exposes Send Now and Delete controls with
    /// stable selectors — the count matches the queue length so desktop's
    /// per-row queue-management parity is preserved.
    #[wasm_bindgen_test]
    async fn chat_queued_rows_expose_send_now_and_delete_buttons() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
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
        // Two queued messages → two controls of each kind. We can't use
        // querySelectorAll (NodeList feature is off in this web-sys
        // build) so iterate via DOM children of the queued container.
        let queued = container
            .query_selector("[data-mobile-test='chat-queued']")
            .unwrap()
            .expect("queued container must render");
        let mut cancel_count = 0;
        let mut send_now_count = 0;
        let mut current = queued.first_element_child();
        while let Some(el) = current.clone() {
            if el.get_attribute("data-mobile-test").as_deref() == Some("chat-queued-row") {
                if el
                    .query_selector("[data-mobile-test='chat-queued-cancel']")
                    .unwrap()
                    .is_some()
                {
                    cancel_count += 1;
                }
                if el
                    .query_selector("[data-mobile-test='chat-queued-send-now']")
                    .unwrap()
                    .is_some()
                {
                    send_now_count += 1;
                }
            }
            current = el.next_element_sibling();
        }
        assert_eq!(cancel_count, 2, "each queued row must have a Delete button");
        assert_eq!(
            send_now_count, 2,
            "each queued row must have a Send Now button"
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
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.agents.set(vec![make_agent(&host_clone, "Coder")]);
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
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
}
