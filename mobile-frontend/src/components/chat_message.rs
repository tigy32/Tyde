use leptos::prelude::*;

use crate::state::ChatMessageEntry;

/// Renders one message in the transcript.
///
/// `data-mobile-test` exposes the sender role for tests
/// (`chat-message-user`, `chat-message-assistant`, etc) so a regression
/// can assert "the second user message says X" without depending on
/// CSS class identity.
#[component]
pub fn ChatMessageView(entry: ChatMessageEntry) -> impl IntoView {
    let (sender_class, sender_test, sender_name): (&'static str, &'static str, String) =
        match &entry.message.sender {
            protocol::MessageSender::User => ("user", "chat-message-user", "You".to_string()),
            protocol::MessageSender::Assistant { agent } => {
                ("assistant", "chat-message-assistant", agent.clone())
            }
            protocol::MessageSender::System => {
                ("system", "chat-message-system", "System".to_string())
            }
            protocol::MessageSender::Warning => {
                ("system", "chat-message-warning", "Warning".to_string())
            }
            protocol::MessageSender::Error => ("error", "chat-message-error", "Error".to_string()),
        };

    let content = entry.message.content.clone();
    let has_reasoning = entry
        .message
        .reasoning
        .as_ref()
        .is_some_and(|r| !r.text.trim().is_empty());
    let reasoning_text = entry
        .message
        .reasoning
        .as_ref()
        .map(|r| r.text.clone())
        .unwrap_or_default();

    let model_info = entry
        .message
        .model_info
        .as_ref()
        .map(|m| m.model.clone())
        .unwrap_or_default();

    let token_usage = entry.message.token_usage.clone();

    let tool_requests = entry.tool_requests;

    view! {
        <div class=format!("chat-message {sender_class}") data-mobile-test=sender_test>
            <div class="message-header">
                <span class="sender-name">{sender_name}</span>
                {
                    let mi = model_info.clone();
                    let mi2 = model_info.clone();
                    view! {
                        <Show when=move || !mi.is_empty()>
                            <span class="model-badge" data-mobile-test="chat-message-model">{mi2.clone()}</span>
                        </Show>
                    }
                }
            </div>

            <Show when=move || has_reasoning>
                <details class="reasoning-block" data-mobile-test="chat-message-reasoning">
                    <summary class="reasoning-label">"Reasoning"</summary>
                    <div class="reasoning-text">{reasoning_text.clone()}</div>
                </details>
            </Show>

            <div class="message-content" data-mobile-test="chat-message-content" inner_html=crate::markdown::render_markdown(&content)></div>

            // Tool requests
            {if tool_requests.is_empty() {
                view! { <div></div> }.into_any()
            } else {
                view! {
                    <div class="tool-cards" data-mobile-test="chat-message-tool-cards">
                        {tool_requests.into_iter().map(|t| {
                            view! { <crate::components::tool_card::ToolCardView entry=t /> }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}

            // Token usage
            {token_usage.map(|usage| {
                view! {
                    <div class="token-usage" data-mobile-test="chat-message-tokens">
                        <span class="token-label">"Tokens: "</span>
                        <span class="token-value">
                            {format!("in:{} out:{}", usage.input_tokens, usage.output_tokens)}
                        </span>
                    </div>
                }
            })}
        </div>
    }
}
