use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::chat_streaming::ChatStreamingView;
use crate::components::task_list::TaskListView;
use crate::state::{AppState, TransientEvent};

use protocol::BackendKind;

struct ScrollListenerHandle {
    element: web_sys::HtmlDivElement,
    callback: Closure<dyn Fn()>,
}

impl ScrollListenerHandle {
    fn remove(self) {
        let _ = self
            .element
            .remove_event_listener_with_callback("scroll", self.callback.as_ref().unchecked_ref());
    }
}

thread_local! {
    static SCROLL_LISTENER_HANDLE: RefCell<Option<ScrollListenerHandle>> = const { RefCell::new(None) };
}

fn clear_scroll_listener() {
    SCROLL_LISTENER_HANDLE.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.remove();
        }
    });
}

#[component]
pub fn ChatView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let has_agent = move || state.active_agent.get().is_some();

    let messages = move || -> Vec<crate::state::ChatMessageEntry> {
        let Some(active_agent) = state.active_agent.get() else {
            return Vec::new();
        };
        let map = state.chat_messages.get();
        map.get(&active_agent.agent_id).cloned().unwrap_or_default()
    };

    let streaming = move || {
        let agent_id = state.active_agent.get()?.agent_id;
        let map = state.streaming_text.get();
        map.get(&agent_id).cloned()
    };

    let task_list = move || {
        let agent_id = state.active_agent.get()?.agent_id;
        let map = state.task_lists.get();
        map.get(&agent_id).cloned()
    };

    let context_breakdown = move || {
        let mut latest_breakdown = None;
        for entry in messages().into_iter().rev() {
            let is_assistant = matches!(
                entry.message.sender,
                protocol::MessageSender::Assistant { .. }
            );
            if !is_assistant {
                continue;
            }

            if let Some(breakdown) = entry.message.context_breakdown.clone() {
                latest_breakdown = Some(breakdown);
                break;
            }

            if entry.message.tool_calls.is_empty() {
                latest_breakdown = None;
                break;
            }
        }
        latest_breakdown
    };

    let transient_events = move || {
        let agent_id = state.active_agent.get()?.agent_id;
        let map = state.transient_events.get();
        map.get(&agent_id).cloned()
    };

    let agent_name = move || -> String {
        let Some(active_agent) = state.active_agent.get() else {
            return String::new();
        };
        let agents = state.agents.get();
        match agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
        {
            Some(a) => a.name.clone(),
            None => "[unknown agent]".to_owned(),
        }
    };

    let agent_backend = move || -> Option<BackendKind> {
        let active_agent = state.active_agent.get()?;
        let agents = state.agents.get();
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .map(|a| a.backend_kind)
    };

    let agent_initializing = move || -> bool {
        let active_agent = match state.active_agent.get() {
            Some(active_agent) => active_agent,
            None => return false,
        };
        state.agents.get().iter().any(|agent| {
            agent.host_id == active_agent.host_id
                && agent.agent_id == active_agent.agent_id
                && !agent.started
                && agent.fatal_error.is_none()
        })
    };

    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    let user_scrolled_up = RwSignal::new(false);
    let show_scroll_btn = RwSignal::new(false);
    on_cleanup(clear_scroll_listener);

    // Track user scroll position to detect manual scroll-up
    let scroll_ref_for_handler = scroll_ref;
    Effect::new(move |_| {
        clear_scroll_listener();
        if let Some(el) = scroll_ref_for_handler.get() {
            let el_clone = el.clone();
            let handler = Closure::<dyn Fn()>::new(move || {
                let scroll_height = el_clone.scroll_height();
                let scroll_top = el_clone.scroll_top();
                let client_height = el_clone.client_height();
                let distance_from_bottom = scroll_height - scroll_top - client_height;
                let is_near_bottom = distance_from_bottom < 80;
                user_scrolled_up.set(!is_near_bottom);
                show_scroll_btn.set(!is_near_bottom);
            });
            let _ = el.add_event_listener_with_callback("scroll", handler.as_ref().unchecked_ref());
            SCROLL_LISTENER_HANDLE.with(|slot| {
                slot.borrow_mut().replace(ScrollListenerHandle {
                    element: el,
                    callback: handler,
                });
            });
        }
    });

    // Auto-scroll effect: whenever messages or streaming change, scroll to bottom
    // (only if user hasn't scrolled up)
    Effect::new(move |_| {
        let _msgs = messages();
        let _stream = streaming();
        if let Some(ss) = _stream.as_ref() {
            let _ = ss.text.get();
            let _ = ss.reasoning.get();
        }
        if user_scrolled_up.get_untracked() {
            return;
        }
        if let Some(el) = scroll_ref.get() {
            request_animation_frame(move || {
                el.set_scroll_top(el.scroll_height());
            });
        }
    });

    let scroll_to_bottom = move |_| {
        if let Some(el) = scroll_ref.get() {
            el.set_scroll_top(el.scroll_height());
            user_scrolled_up.set(false);
            show_scroll_btn.set(false);
        }
    };

    let has_messages = move || !messages().is_empty();

    view! {
        <div class="chat-view">
            <Show
                when=has_agent
                fallback=move || {
                    view! {
                        <div class="chat-welcome">
                            <div class="chat-welcome-inner">
                                <img class="chat-welcome-icon" src="icon.png" alt="Tyde" />
                                <h2 class="chat-welcome-title">"Tyde"</h2>
                                <p class="chat-welcome-subtitle">"Send a message to start a conversation"</p>
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
                            BackendKind::Gemini => ("backend-badge gemini", "Gemini"),
                        };
                        view! { <span class=badge_class>{label}</span> }
                    })}
                </div>
                {move || {
                    view! {
                        <TaskListView
                            task_list=task_list()
                            context_breakdown=context_breakdown()
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
                        // Show welcome hint when chat is empty and no streaming
                        {move || {
                            if !has_messages() && streaming().is_none() {
                                Some(view! {
                                    <div class="chat-empty-hint">
                                        <p>"Type a message to start the conversation"</p>
                                    </div>
                                })
                            } else {
                                None
                            }
                        }}

                        {move || {
                            messages().into_iter().map(|entry| {
                                view! { <ChatMessageView entry=entry /> }
                            }).collect::<Vec<_>>()
                        }}

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
                            streaming().map(|ss| view! { <ChatStreamingView streaming=ss /> })
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
            <ChatInput />
        </div>
    }
}
