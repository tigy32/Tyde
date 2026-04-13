use leptos::prelude::*;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::chat_streaming::ChatStreamingView;
use crate::components::task_list::TaskListView;
use crate::state::{AppState, TransientEvent};

#[component]
pub fn ChatView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let has_agent = move || state.active_agent_id.get().is_some();

    let messages = move || -> Vec<crate::state::ChatMessageEntry> {
        let Some(agent_id) = state.active_agent_id.get() else {
            log::error!("messages() called with no active_agent_id");
            return Vec::new();
        };
        let map = state.chat_messages.get();
        map.get(&agent_id).cloned().unwrap_or_default()
    };

    let streaming = move || {
        let agent_id = state.active_agent_id.get()?;
        let map = state.streaming_text.get();
        map.get(&agent_id).cloned()
    };

    let task_list = move || {
        let agent_id = state.active_agent_id.get()?;
        let map = state.task_lists.get();
        map.get(&agent_id).cloned()
    };

    let transient_events = move || {
        let agent_id = state.active_agent_id.get()?;
        let map = state.transient_events.get();
        map.get(&agent_id).cloned()
    };

    let agent_name = move || -> String {
        let Some(agent_id) = state.active_agent_id.get() else {
            return String::new();
        };
        let agents = state.agents.get();
        match agents.iter().find(|a| a.agent_id == agent_id) {
            Some(a) => a.name.clone(),
            None => {
                log::error!("active_agent_id {:?} not found in agents list", agent_id);
                format!("[unknown agent {:?}]", agent_id)
            }
        }
    };

    let scroll_ref = NodeRef::<leptos::html::Div>::new();

    // Auto-scroll effect: whenever messages or streaming change, scroll to bottom
    Effect::new(move |_| {
        let _msgs = messages();
        let _stream = streaming();
        if let Some(el) = scroll_ref.get() {
            // Use request_animation_frame to ensure DOM has updated
            request_animation_frame(move || {
                el.set_scroll_top(el.scroll_height());
            });
        }
    });

    view! {
        <div class="chat-view">
            <Show
                when=has_agent
                fallback=|| view! {
                    <div class="chat-empty">
                        <p class="chat-empty-text">"Select an agent to start chatting"</p>
                    </div>
                }
            >
                <div class="chat-agent-header">
                    <span class="chat-agent-name">{agent_name}</span>
                </div>
                <div class="chat-messages" node_ref=scroll_ref>
                    {move || {
                        messages().into_iter().map(|entry| {
                            view! { <ChatMessageView entry=entry /> }
                        }).collect::<Vec<_>>()
                    }}

                    {move || {
                        transient_events().map(|events| {
                            events.into_iter().map(|ev| {
                                let (class, text) = match ev {
                                    TransientEvent::OperationCancelled { message } => {
                                        ("transient-notice transient-cancelled", format!("Operation cancelled: {message}"))
                                    }
                                    TransientEvent::RetryAttempt { attempt, max_retries, error, backoff_ms } => {
                                        ("transient-notice transient-retry", format!("Retrying ({attempt}/{max_retries}): {error}. Waiting {backoff_ms}ms\u{2026}"))
                                    }
                                };
                                view! { <div class=class>{text}</div> }
                            }).collect::<Vec<_>>()
                        })
                    }}

                    {move || {
                        streaming().map(|ss| view! { <ChatStreamingView streaming=ss /> })
                    }}

                    {move || {
                        task_list().map(|tl| view! { <TaskListView task_list=tl /> })
                    }}
                </div>
                <ChatInput />
            </Show>
        </div>
    }
}
