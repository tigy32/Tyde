use leptos::prelude::*;
use protocol::MessageSender;

use crate::components::tool_card::ToolCardView;
use crate::highlight::highlight_code_blocks;
use crate::markdown::render_markdown;
use crate::state::ChatMessageEntry;

#[component]
pub fn ChatMessageView(entry: ChatMessageEntry) -> impl IntoView {
    let msg = &entry.message;

    let (card_class, sender_label, is_user, is_assistant) = match &msg.sender {
        MessageSender::User => ("chat-card chat-card-user", "You".to_owned(), true, false),
        MessageSender::Assistant { agent } => {
            ("chat-card chat-card-assistant", agent.clone(), false, true)
        }
        MessageSender::System => (
            "chat-card chat-card-system",
            "System".to_owned(),
            false,
            false,
        ),
        MessageSender::Warning => (
            "chat-card chat-card-warning",
            "Warning".to_owned(),
            false,
            false,
        ),
        MessageSender::Error => (
            "chat-card chat-card-error",
            "Error".to_owned(),
            false,
            false,
        ),
    };

    let content = msg.content.clone();
    let timestamp = msg.timestamp;
    let model_info = msg.model_info.clone();
    let token_usage = msg.token_usage.clone();
    let reasoning = msg.reasoning.clone();
    let images = msg.images.clone();
    let tool_requests = entry.tool_requests;

    let time_display = format_relative_time(timestamp);

    // Render content as markdown for assistant messages, plain for user messages
    let content_html = if is_user {
        // User messages: escape HTML, preserve newlines
        let escaped = content
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        format!("<span class=\"user-text\">{escaped}</span>")
    } else {
        render_markdown(&content)
    };

    // Copy state for footer copy button
    let copy_state = RwSignal::new("copy"); // "copy" | "copied" | "failed"
    let content_for_copy = content.clone();

    let on_copy = move |_| {
        let text = content_for_copy.clone();
        let state = copy_state;
        wasm_bindgen_futures::spawn_local(async move {
            let window = web_sys::window().unwrap();
            let navigator = window.navigator();
            let clipboard = navigator.clipboard();
            match wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&text)).await {
                Ok(_) => {
                    state.set("copied");
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1200);
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    state.set("copy");
                }
                Err(_) => {
                    state.set("failed");
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1200);
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    state.set("copy");
                }
            }
        });
    };

    // Build footer for assistant messages
    let show_footer = is_assistant;
    let footer_sender_label = sender_label.clone();
    let footer_model = model_info.clone();
    let footer_tokens = token_usage.clone();
    let footer_time = time_display.clone();
    let footer_content_empty = content.is_empty();

    // Pre-compute the token badge data for the footer
    let token_badge = footer_tokens.as_ref().map(|tu| {
        let input_base = tu.input_tokens;
        let cached_hits = tu.cached_prompt_tokens.unwrap_or(0);
        let cache_writes = tu.cache_creation_input_tokens.unwrap_or(0);
        let reasoning = tu.reasoning_tokens.unwrap_or(0);

        // Display total prompt-side tokens (base + cache hits + cache writes)
        let display_input = input_base + cached_hits + cache_writes;
        // output_tokens already includes reasoning per contract
        let display_output = tu.output_tokens;

        let input_text = if cached_hits > 0 {
            format!(
                "\u{2191}{} (cached {})",
                format_compact(display_input),
                format_compact(cached_hits)
            )
        } else {
            format!("\u{2191}{}", format_compact(display_input))
        };

        let output_text = if reasoning > 0 {
            format!(
                "\u{2193}{} (reasoning {})",
                format_compact(display_output),
                format_compact(reasoning)
            )
        } else {
            format!("\u{2193}{}", format_compact(display_output))
        };

        // Detailed hover tooltip
        let tooltip = format!(
            "Input {} (base {} + cache hits {} + cache writes {}), Output {} (incl. reasoning {})",
            format_compact(display_input),
            format_compact(input_base),
            format_compact(cached_hits),
            format_compact(cache_writes),
            format_compact(display_output),
            format_compact(reasoning),
        );

        (input_text, output_text, tooltip)
    });

    view! {
        <div class=card_class>
            // Header row: sender name + model badge (for assistant) or just "You" (for user)
            <div class="chat-card-header">
                <span class="chat-card-sender">{sender_label}</span>
                {model_info.map(|mi| view! {
                    <span class="chat-card-model">{mi.model}</span>
                })}
                <span class="chat-card-time">{time_display}</span>
            </div>

            // Reasoning (collapsible)
            {reasoning.map(|r| {
                let text = r.text;
                let token_count = r.tokens;
                view! {
                    <details class="chat-card-reasoning">
                        <summary>
                            <span class="reasoning-icon">"💭"</span>
                            " Thinking"
                            {token_count.map(|t| view! {
                                <span class="reasoning-tokens">{format!(" ({} tokens)", format_compact(t))}</span>
                            })}
                        </summary>
                        <pre class="reasoning-content">{text}</pre>
                    </details>
                }
            })}

            // Message content (rendered as markdown) — hide if empty
            {(!content.is_empty()).then(|| {
                let body_ref: NodeRef<leptos::html::Div> = NodeRef::new();
                Effect::new(move |_| {
                    if let Some(el) = body_ref.get() {
                        highlight_code_blocks(&el);
                    }
                });
                view! {
                    <div
                        class="chat-card-body"
                        node_ref=body_ref
                        inner_html=content_html.clone()
                    ></div>
                }
            })}

            // Images
            {images.and_then(|imgs| {
                if imgs.is_empty() {
                    return None;
                }
                Some(view! {
                    <div class="chat-card-images">
                        {imgs.into_iter().map(|img| {
                            let src = format!("data:{};base64,{}", img.media_type, img.data);
                            view! {
                                <img class="chat-card-image" src=src alt="Attached image" loading="lazy" />
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                })
            })}

            // Tool cards
            {(!tool_requests.is_empty()).then(|| {
                let cards = tool_requests.into_iter().map(|tr| {
                    view! { <ToolCardView entry=tr /> }
                }).collect::<Vec<_>>();
                view! { <div class="chat-card-tools">{cards}</div> }
            })}

            // Footer meta bar (assistant only)
            {show_footer.then(move || {
                let model_display = footer_model.as_ref().map(|mi| mi.model.clone());
                let agent_display = footer_sender_label.clone();
                let badge = token_badge.clone();
                let badge_tooltip = badge.as_ref().map(|(_, _, t)| t.clone()).unwrap_or_default();

                view! {
                    <div class="chat-card-footer">
                        <span class="token-badge" title=badge_tooltip>
                            // Model name
                            {model_display.map(|m| view! {
                                <span class="token-stat token-stat-model">{m}</span>
                                <span class="token-sep">"·"</span>
                            })}
                            // Agent name
                            <span class="token-stat token-stat-agent">{agent_display}</span>
                            // Token counts
                            {badge.map(|(input_text, output_text, _tooltip)| view! {
                                <span class="token-sep">"·"</span>
                                <span class="token-stat token-stat-input">{input_text}</span>
                                <span class="token-sep">"·"</span>
                                <span class="token-stat token-stat-output">{output_text}</span>
                            })}
                        </span>
                        <span class="chat-card-footer-right">
                            <span class="footer-time">{footer_time}</span>
                            {(!footer_content_empty).then(move || view! {
                                <button
                                    class=move || {
                                        match copy_state.get() {
                                            "copied" => "footer-copy-btn copied",
                                            "failed" => "footer-copy-btn copy-failed",
                                            _ => "footer-copy-btn",
                                        }
                                    }
                                    title="Copy message"
                                    on:click=on_copy
                                >
                                    {move || match copy_state.get() {
                                        "copied" => "\u{2713}",
                                        "failed" => "!",
                                        _ => "\u{29C9}",
                                    }}
                                </button>
                            })}
                        </span>
                    </div>
                }
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
        format!("{mins}m ago")
    } else if diff_secs < 86400 {
        let hours = diff_secs / 3600;
        format!("{hours}h ago")
    } else {
        let days = diff_secs / 86400;
        format!("{days}d ago")
    }
}

fn format_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
