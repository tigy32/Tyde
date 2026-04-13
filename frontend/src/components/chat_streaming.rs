use leptos::prelude::*;

use crate::state::StreamingState;

#[component]
pub fn ChatStreamingView(streaming: StreamingState) -> impl IntoView {
    let has_text = !streaming.text.is_empty();
    let has_reasoning = !streaming.reasoning.is_empty();
    let text = streaming.text;
    let reasoning = streaming.reasoning;
    let agent_name = streaming.agent_name;
    let model = streaming.model;

    view! {
        <div class="chat-bubble chat-bubble-assistant chat-streaming">
            <div class="chat-bubble-header">
                <span class="chat-sender">{agent_name}</span>
                {model.map(|m| view! {
                    <span class="chat-model-badge">{m}</span>
                })}
                <span class="streaming-indicator">"●"</span>
            </div>
            <Show when=move || has_reasoning>
                <details class="chat-reasoning" open>
                    <summary>"Reasoning"</summary>
                    <p class="chat-reasoning-text">{reasoning.clone()}</p>
                </details>
            </Show>
            <div class="chat-bubble-content">
                <Show
                    when=move || has_text
                    fallback=|| view! { <p class="chat-thinking">"Thinking..."</p> }
                >
                    <p class="chat-text">{text.clone()}<span class="streaming-cursor">"|"</span></p>
                </Show>
            </div>
        </div>
    }
}
