use leptos::prelude::*;

use crate::state::ChatMessageEntry;

/// Resolve the THIS-TURN token usage for a chat row. `token_usage` is the
/// authoritative this-turn figure; `turn_token_usage` refines it: `Known`
/// carries the same this-turn figure explicitly (never the cumulative
/// `agent_total`), and `Unavailable` means the backend reported nothing, so
/// the row renders no token line rather than a fake-zero one.
fn this_turn_token_usage(message: &protocol::ChatMessage) -> Option<protocol::TokenUsage> {
    match &message.turn_token_usage {
        Some(protocol::TurnTokenUsage::Unavailable { .. }) => None,
        Some(protocol::TurnTokenUsage::Known { this_turn, .. }) => Some((**this_turn).clone()),
        None => message.token_usage.clone(),
    }
}

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

    let token_usage = this_turn_token_usage(&entry.message);

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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::{
        ChatMessage, MessageSender, TokenUsage, TokenUsageUnavailableReason, TurnTokenUsage,
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

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn assistant_entry(
        token_usage: Option<TokenUsage>,
        turn_token_usage: Option<TurnTokenUsage>,
    ) -> ChatMessageEntry {
        ChatMessageEntry {
            message: ChatMessage {
                message_id: None,
                timestamp: 0,
                sender: MessageSender::Assistant {
                    agent: "codex".to_owned(),
                },
                content: "hello".to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage,
                turn_token_usage,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        }
    }

    fn mount(entry: ChatMessageEntry) -> HtmlElement {
        let container = make_container();
        mount_to(container.clone(), move || {
            view! { <ChatMessageView entry=entry.clone() /> }
        })
        .forget();
        container
    }

    /// `TurnTokenUsage::Known` renders the THIS-TURN figure, never the
    /// cumulative `agent_total` carried alongside it.
    #[wasm_bindgen_test]
    async fn mobile_chat_known_shows_per_turn_figure() {
        let entry = assistant_entry(
            Some(usage(4200, 1300)),
            Some(TurnTokenUsage::Known {
                this_turn: Box::new(usage(4200, 1300)),
                agent_total: Box::new(usage(999_000, 888_000)),
            }),
        );
        let container = mount(entry);
        next_tick().await;

        let tokens = container
            .query_selector("[data-mobile-test='chat-message-tokens']")
            .unwrap()
            .expect("Known turn renders a token line")
            .text_content()
            .unwrap_or_default();
        assert!(
            tokens.contains("in:4200") && tokens.contains("out:1300"),
            "Known turn shows the this-turn figure: {tokens}"
        );
        assert!(
            !tokens.contains("999000") && !tokens.contains("888000"),
            "cumulative agent_total must not leak into the per-turn line: {tokens}"
        );
    }

    /// `TurnTokenUsage::Unavailable` means the backend reported nothing this
    /// turn; the mobile row must render no token line rather than a fake-zero
    /// one, even though `token_usage` carries zeros for compatibility.
    #[wasm_bindgen_test]
    async fn mobile_chat_unavailable_renders_no_fake_zero() {
        let entry = assistant_entry(
            Some(usage(0, 0)),
            Some(TurnTokenUsage::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            }),
        );
        let container = mount(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='chat-message-tokens']")
                .unwrap()
                .is_none(),
            "Unavailable turn must not render a token line"
        );
        let body = container.text_content().unwrap_or_default();
        assert!(
            !body.contains("in:0 out:0"),
            "Unavailable turn must not show a fake-zero token figure: {body}"
        );
    }
}
