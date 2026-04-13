use leptos::prelude::*;
use protocol::MessageSender;

use crate::components::tool_card::ToolCardView;
use crate::state::ChatMessageEntry;

#[component]
pub fn ChatMessageView(entry: ChatMessageEntry) -> impl IntoView {
    let msg = &entry.message;

    let (bubble_class, sender_label) = match &msg.sender {
        MessageSender::User => ("chat-bubble chat-bubble-user", "You".to_owned()),
        MessageSender::Assistant { agent } => {
            ("chat-bubble chat-bubble-assistant", agent.clone())
        }
        MessageSender::System => ("chat-bubble chat-bubble-system", "System".to_owned()),
        MessageSender::Warning => ("chat-bubble chat-bubble-warning", "Warning".to_owned()),
        MessageSender::Error => ("chat-bubble chat-bubble-error", "Error".to_owned()),
    };

    let content = msg.content.clone();
    let timestamp = msg.timestamp;
    let model_info = msg.model_info.clone();
    let token_usage = msg.token_usage.clone();
    let reasoning = msg.reasoning.clone();
    let tool_requests = entry.tool_requests;

    let time_display = format_relative_time(timestamp);

    view! {
        <div class=bubble_class>
            <div class="chat-bubble-header">
                <span class="chat-sender">{sender_label}</span>
                <span class="chat-time">{time_display}</span>
                {model_info.map(|mi| view! {
                    <span class="chat-model-badge">{mi.model}</span>
                })}
            </div>
            <div class="chat-bubble-content">
                <p class="chat-text">{content}</p>
            </div>
            {reasoning.map(|r| {
                let text = r.text;
                view! {
                    <details class="chat-reasoning">
                        <summary>"Reasoning"</summary>
                        <p class="chat-reasoning-text">{text}</p>
                    </details>
                }
            })}
            {(!tool_requests.is_empty()).then(|| {
                let cards = tool_requests.into_iter().map(|tr| {
                    view! { <ToolCardView entry=tr /> }
                }).collect::<Vec<_>>();
                view! { <div class="chat-tool-cards">{cards}</div> }
            })}
            {token_usage.map(|tu| view! {
                <div class="chat-token-usage">
                    <span class="token-label">"Tokens: "</span>
                    <span class="token-value">{format!("{}in / {}out", tu.input_tokens, tu.output_tokens)}</span>
                    {tu.cached_prompt_tokens.map(|c| view! {
                        <span class="token-cached">{format!(" ({c} cached)")}</span>
                    })}
                </div>
            })}
        </div>
    }
}

fn format_relative_time(timestamp_ms: u64) -> String {
    let now_ms = js_sys::Date::now() as u64;
    if timestamp_ms == 0 {
        return String::new();
    }
    let diff_secs = now_ms.saturating_sub(timestamp_ms) / 1000;
    if diff_secs < 60 {
        "just now".to_owned()
    } else if diff_secs < 3600 {
        let mins = diff_secs / 60;
        format!("{mins} min ago")
    } else if diff_secs < 86400 {
        let hours = diff_secs / 3600;
        format!("{hours}h ago")
    } else {
        let days = diff_secs / 86400;
        format!("{days}d ago")
    }
}
