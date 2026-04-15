use leptos::prelude::*;

use crate::components::tool_card::ToolCardView;
use crate::markdown::render_markdown;
use crate::state::StreamingState;

#[component]
pub fn ChatStreamingView(streaming: StreamingState) -> impl IntoView {
    let text = streaming.text;
    let reasoning = streaming.reasoning;
    let tool_requests = streaming.tool_requests;
    let agent_name = streaming.agent_name;
    let model = streaming.model;

    view! {
        <div class="chat-card chat-card-assistant chat-card-streaming">
            <div class="chat-card-header">
                <span class="chat-card-sender">{agent_name}</span>
                {model.map(|m| view! {
                    <span class="chat-card-model">{m}</span>
                })}
                <span class="streaming-dot"></span>
            </div>
            {move || {
                let current_reasoning = reasoning.get();
                if current_reasoning.is_empty() {
                    None
                } else {
                    Some(view! {
                        <details class="chat-card-reasoning" open>
                            <summary>
                                <span class="reasoning-icon">"💭"</span>
                                " Thinking"
                            </summary>
                            <pre class="reasoning-content">{current_reasoning}</pre>
                        </details>
                    })
                }
            }}
            <div class="chat-card-body">
                {move || {
                    let current = text.get();
                    if current.is_empty() {
                        view! { <p class="chat-card-thinking">"Thinking\u{2026}"</p> }.into_any()
                    } else {
                        let html = render_markdown(&current);
                        view! {
                            <>
                                <div inner_html=html></div>
                                <span class="streaming-cursor"></span>
                            </>
                        }.into_any()
                    }
                }}
            </div>
            {move || {
                let tools = tool_requests.get();
                if tools.is_empty() {
                    None
                } else {
                    Some(view! {
                        <div class="chat-card-tools">
                            {tools.into_iter().map(|tr| {
                                view! { <ToolCardView entry=tr /> }
                            }).collect::<Vec<_>>()}
                        </div>
                    })
                }
            }}
        </div>
    }
}
