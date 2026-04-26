use leptos::prelude::*;
use protocol::{AgentId, MessageSender};

use crate::components::tool_card::ToolCardView;
use crate::highlight::highlight_code_blocks;
use crate::markdown::render_markdown;
use crate::state::{AppState, ChatMessageEntry};

/// Render a single chat row, addressed by `(agent_id, idx)` so the keyed
/// `<For>` in `ChatView` can preserve row identity across appends. The row
/// reads the underlying `ChatMessageEntry` reactively from `state.chat_messages`,
/// so in-place mutations (e.g. `ToolRequest` adding tool cards to an existing
/// message — see `dispatch.rs::ChatEvent::ToolRequest`) project through.
///
/// `agent_id` and `idx` are fixed at row creation: an agent switch produces a
/// fresh key and remounts. Within a single agent, appends preserve the rows
/// 0..len() and only mount the new tail row.
#[component]
pub fn ChatMessageView(agent_id: AgentId, idx: usize) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Source of truth for this row. Re-fires on any chat_messages update for
    // this agent — but the per-field Memos below short-circuit when their
    // narrow inputs are unchanged, so unrelated updates (e.g. another row's
    // tool_request) do not re-render markdown or rebuild static fields.
    let entry_agent_id = agent_id.clone();
    let entry: Signal<Option<ChatMessageEntry>> = Signal::derive(move || {
        state
            .chat_messages
            .with(|m| m.get(&entry_agent_id).and_then(|v| v.get(idx).cloned()))
    });

    // Card class / sender label / role flags. These can change only if the
    // entry at this (agent_id, idx) is replaced — which today only happens at
    // clear_host_runtime, which drops the whole agent and unmounts the row.
    // We still derive them through Memos so the view stays a clean projection.
    let card_meta: Memo<(String, String, bool, bool)> = Memo::new(move |_| {
        let Some(e) = entry.get() else {
            return ("chat-card".to_owned(), String::new(), false, false);
        };
        match &e.message.sender {
            MessageSender::User => (
                "chat-card chat-card-user".to_owned(),
                "You".to_owned(),
                true,
                false,
            ),
            MessageSender::Assistant { agent } => (
                "chat-card chat-card-assistant".to_owned(),
                agent.clone(),
                false,
                true,
            ),
            MessageSender::System => (
                "chat-card chat-card-system".to_owned(),
                "System".to_owned(),
                false,
                false,
            ),
            MessageSender::Warning => (
                "chat-card chat-card-warning".to_owned(),
                "Warning".to_owned(),
                false,
                false,
            ),
            MessageSender::Error => (
                "chat-card chat-card-error".to_owned(),
                "Error".to_owned(),
                false,
                false,
            ),
        }
    });

    // Memo on (is_user, content): tuple of primitives with PartialEq, so the
    // Memo short-circuits when neither changes. For finalized messages this
    // computes once; for sibling chat_messages updates it returns cached.
    let content_data: Memo<(bool, String)> = Memo::new(move |_| {
        let Some(e) = entry.get() else {
            return (false, String::new());
        };
        let is_user = matches!(e.message.sender, MessageSender::User);
        (is_user, e.message.content)
    });

    let content_html: Memo<String> = Memo::new(move |_| {
        let (is_user, content) = content_data.get();
        if content.is_empty() {
            return String::new();
        }
        if is_user {
            let escaped = content
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!("<span class=\"user-text\">{escaped}</span>")
        } else {
            render_markdown(&content)
        }
    });

    let timestamp_memo: Memo<u64> =
        Memo::new(move |_| entry.get().map(|e| e.message.timestamp).unwrap_or(0));

    let model_memo: Memo<Option<String>> = Memo::new(move |_| {
        entry
            .get()
            .and_then(|e| e.message.model_info.map(|mi| mi.model))
    });

    let copy_state = RwSignal::new("copy");

    // Copy the *current* content at click time, not a row-creation snapshot.
    let on_copy_agent = agent_id.clone();
    let on_copy = move |_| {
        let state = expect_context::<AppState>();
        let Some(text) = state.chat_messages.with_untracked(|m| {
            m.get(&on_copy_agent)
                .and_then(|v| v.get(idx))
                .map(|e| e.message.content.clone())
        }) else {
            return;
        };
        let cs = copy_state;
        wasm_bindgen_futures::spawn_local(async move {
            let window = web_sys::window().unwrap();
            let navigator = window.navigator();
            let clipboard = navigator.clipboard();
            match wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&text)).await {
                Ok(_) => {
                    cs.set("copied");
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1200);
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    cs.set("copy");
                }
                Err(_) => {
                    cs.set("failed");
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1200);
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    cs.set("copy");
                }
            }
        });
    };

    let body_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    // Re-highlight whenever the rendered HTML actually changes (Memo guarantees
    // this only fires when content changes, not on sibling chat_messages
    // updates).
    Effect::new(move |_| {
        let _ = content_html.get();
        if let Some(el) = body_ref.get() {
            highlight_code_blocks(&el);
        }
    });

    view! {
        <div class=move || card_meta.with(|(c, _, _, _)| c.clone())>
            <div class="chat-card-header">
                <span class="chat-card-sender">{move || card_meta.with(|(_, s, _, _)| s.clone())}</span>
                {move || model_memo.get().map(|m| view! {
                    <span class="chat-card-model">{m}</span>
                })}
                <span class="chat-card-time">{move || format_relative_time(timestamp_memo.get())}</span>
            </div>

            // Reasoning (collapsible)
            {move || {
                entry.get().and_then(|e| e.message.reasoning).map(|r| {
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
                })
            }}

            // Body — hidden when content is empty
            {move || {
                let html = content_html.get();
                if html.is_empty() {
                    None
                } else {
                    Some(view! {
                        <div
                            class="chat-card-body"
                            node_ref=body_ref
                            inner_html=html
                        ></div>
                    })
                }
            }}

            // Images
            {move || {
                entry.get().and_then(|e| e.message.images).and_then(|imgs| {
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
                })
            }}

            // Tool cards — read reactively because ToolRequest events mutate
            // the in-place tool_requests vec (see dispatch.rs:1424).
            {move || {
                let tools = entry.get().map(|e| e.tool_requests).unwrap_or_default();
                if tools.is_empty() {
                    return None;
                }
                Some(view! {
                    <div class="chat-card-tools">
                        {tools.into_iter().map(|tr| {
                            view! { <ToolCardView entry=tr /> }
                        }).collect::<Vec<_>>()}
                    </div>
                })
            }}

            // Footer (assistant only)
            {move || {
                let is_assistant = card_meta.with(|(_, _, _, ia)| *ia);
                if !is_assistant {
                    return None;
                }
                let e = entry.get()?;
                let model_display = e.message.model_info.as_ref().map(|mi| mi.model.clone());
                let agent_display = match &e.message.sender {
                    MessageSender::Assistant { agent } => agent.clone(),
                    _ => String::new(),
                };
                let badge = e.message.token_usage.as_ref().map(token_badge_data);
                let badge_tooltip = badge.as_ref().map(|(_, _, t)| t.clone()).unwrap_or_default();
                let footer_time = format_relative_time(e.message.timestamp);
                let footer_content_empty = e.message.content.is_empty();
                let on_copy_handler = on_copy.clone();

                Some(view! {
                    <div class="chat-card-footer">
                        <span class="token-badge" title=badge_tooltip>
                            {model_display.map(|m| view! {
                                <span class="token-stat token-stat-model">{m}</span>
                                <span class="token-sep">"·"</span>
                            })}
                            <span class="token-stat token-stat-agent">{agent_display}</span>
                            {badge.map(|(input_text, output_text, _)| view! {
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
                                    on:click=on_copy_handler
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
                })
            }}
        </div>
    }
}

fn token_badge_data(tu: &protocol::TokenUsage) -> (String, String, String) {
    let input_base = tu.input_tokens;
    let cached_hits = tu.cached_prompt_tokens.unwrap_or(0);
    let cache_writes = tu.cache_creation_input_tokens.unwrap_or(0);
    let reasoning = tu.reasoning_tokens.unwrap_or(0);

    let display_input = input_base + cached_hits + cache_writes;
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
