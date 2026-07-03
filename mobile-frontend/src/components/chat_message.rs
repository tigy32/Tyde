use leptos::prelude::*;

use crate::state::ChatMessageEntry;

/// The per-request token usage shown by default on a mobile chat row, or `None`
/// when the backend didn't report it (no fake-zero figure). Mirrors the desktop
/// footer default.
fn request_scope(message: &protocol::ChatMessage) -> Option<protocol::TokenUsage> {
    message
        .token_usage
        .as_ref()
        .and_then(|usage| usage.request.known_usage().cloned())
}

/// The turn + cumulative scopes, shown only inside the expandable details so
/// the wider cumulative figure never dominates the row by default. Only scopes
/// the backend actually reported are returned; unavailable/absent scopes are
/// omitted (never a fake zero).
fn detail_scopes(message: &protocol::ChatMessage) -> Vec<(&'static str, protocol::TokenUsage)> {
    let Some(usage) = message.token_usage.as_ref() else {
        return Vec::new();
    };
    [("Turn", &usage.turn), ("Cumulative", &usage.cumulative)]
        .into_iter()
        .filter_map(|(label, scope)| scope.known_usage().map(|u| (label, u.clone())))
        .collect()
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

    let request_usage = request_scope(&entry.message);
    let detail_scopes = detail_scopes(&entry.message);

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

            // Token usage — the request scope shows by default; the wider turn +
            // cumulative scopes sit behind an explicit tap-to-expand so they can
            // never dominate the row. Collapsed details are not rendered at all,
            // so the default row is genuinely request-only.
            {(request_usage.is_some() || !detail_scopes.is_empty()).then(|| {
                let expanded = RwSignal::new(false);
                let summary_label = match &request_usage {
                    Some(u) => format!("Request: in:{} out:{}", u.input_tokens, u.output_tokens),
                    None => "Tokens".to_owned(),
                };
                let has_details = !detail_scopes.is_empty();
                let detail_data = detail_scopes;
                view! {
                    <div class="token-usage" data-mobile-test="chat-message-tokens">
                        {if has_details {
                            let summary_label = summary_label.clone();
                            view! {
                                <button
                                    class="token-summary"
                                    type="button"
                                    data-mobile-test="chat-message-token-toggle"
                                    aria-expanded=move || if expanded.get() { "true" } else { "false" }
                                    on:click=move |_| expanded.update(|e| *e = !*e)
                                >
                                    <span class="token-value" data-mobile-test="chat-message-token-summary">
                                        {summary_label}
                                    </span>
                                    <span
                                        class="token-chevron"
                                        class:open=move || expanded.get()
                                    >
                                        "\u{25B8}"
                                    </span>
                                </button>
                            }.into_any()
                        } else {
                            view! {
                                <div class="token-scope">
                                    <span class="token-value" data-mobile-test="chat-message-token-summary">
                                        {summary_label.clone()}
                                    </span>
                                </div>
                            }.into_any()
                        }}
                        {move || {
                            if !expanded.get() {
                                return None;
                            }
                            let lines = detail_data
                                .iter()
                                .map(|(label, usage)| view! {
                                    <div class="token-scope token-detail" data-mobile-test="chat-message-token-detail">
                                        <span class="token-label">{format!("{label}: ")}</span>
                                        <span class="token-value">
                                            {format!("in:{} out:{}", usage.input_tokens, usage.output_tokens)}
                                        </span>
                                    </div>
                                })
                                .collect::<Vec<_>>();
                            Some(view! {
                                <div class="token-details" data-mobile-test="chat-message-token-details">
                                    {lines}
                                </div>
                            })
                        }}
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
        ChatMessage, MessageSender, MessageTokenUsage, TokenUsage, TokenUsageUnavailableReason,
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

    fn assistant_entry(token_usage: Option<MessageTokenUsage>) -> ChatMessageEntry {
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

    fn detail_count(container: &HtmlElement) -> u32 {
        container
            .query_selector_all("[data-mobile-test='chat-message-token-detail']")
            .unwrap()
            .length()
    }

    /// A message that carries all three scopes shows the REQUEST figure only by
    /// default: the turn + cumulative scopes live behind the tap-to-expand and
    /// are not rendered (not just hidden) while collapsed, so the wide
    /// cumulative figure can never dominate the row.
    #[wasm_bindgen_test]
    async fn mobile_chat_shows_request_only_by_default() {
        let entry = assistant_entry(Some(
            MessageTokenUsage::request_and_turn_known(usage(4200, 1300), usage(5000, 1500))
                .with_cumulative(usage(999_000, 888_000)),
        ));
        let container = mount(entry);
        next_tick().await;

        let tokens = container
            .query_selector("[data-mobile-test='chat-message-tokens']")
            .unwrap()
            .expect("known request scope renders a token block")
            .text_content()
            .unwrap_or_default();
        assert!(
            tokens.contains("Request:")
                && tokens.contains("in:4200")
                && tokens.contains("out:1300"),
            "the request figure shows by default: {tokens}"
        );
        // Turn + cumulative must NOT be present in the DOM while collapsed.
        assert_eq!(
            detail_count(&container),
            0,
            "no detail lines are rendered by default"
        );
        assert!(
            !tokens.contains("Turn:")
                && !tokens.contains("Cumulative:")
                && !tokens.contains("999000"),
            "turn/cumulative must not show until expanded: {tokens}"
        );
        // The expand affordance is present because there are hidden scopes.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-message-token-toggle']")
                .unwrap()
                .is_some(),
            "an expand affordance must be offered when turn/cumulative exist"
        );
    }

    /// Tapping the token toggle reveals the turn + cumulative scopes, each
    /// labeled — the explicit, tap-driven (not hover) details affordance.
    #[wasm_bindgen_test]
    async fn mobile_chat_expands_to_reveal_turn_and_cumulative() {
        let entry = assistant_entry(Some(
            MessageTokenUsage::request_and_turn_known(usage(4200, 1300), usage(5000, 1500))
                .with_cumulative(usage(999_000, 888_000)),
        ));
        let container = mount(entry);
        next_tick().await;

        let toggle = container
            .query_selector("[data-mobile-test='chat-message-token-toggle']")
            .unwrap()
            .expect("expand affordance present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("false"),
            "collapsed by default"
        );

        toggle.click();
        next_tick().await;

        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("true"),
            "tapping the toggle expands the details"
        );
        assert_eq!(
            detail_count(&container),
            2,
            "turn + cumulative reveal on expand"
        );
        let tokens = container
            .query_selector("[data-mobile-test='chat-message-tokens']")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap_or_default();
        assert!(
            tokens.contains("Turn:") && tokens.contains("in:5000") && tokens.contains("out:1500"),
            "turn scope revealed on expand: {tokens}"
        );
        assert!(
            tokens.contains("Cumulative:")
                && tokens.contains("in:999000")
                && tokens.contains("out:888000"),
            "cumulative scope revealed on expand: {tokens}"
        );
    }

    /// A request-only message shows just the request line with NO expand
    /// affordance — turn/cumulative are unavailable, so there is nothing to
    /// reveal and no fake zeros appear.
    #[wasm_bindgen_test]
    async fn mobile_chat_request_only_has_no_expand_affordance() {
        let entry = assistant_entry(Some(MessageTokenUsage::request_known(usage(4200, 1300))));
        let container = mount(entry);
        next_tick().await;

        let tokens = container
            .query_selector("[data-mobile-test='chat-message-tokens']")
            .unwrap()
            .expect("request scope renders a token block")
            .text_content()
            .unwrap_or_default();
        assert!(
            tokens.contains("Request:") && tokens.contains("in:4200"),
            "request scope is shown: {tokens}"
        );
        assert!(
            !tokens.contains("Turn:") && !tokens.contains("Cumulative:"),
            "unavailable scopes must be omitted, not shown: {tokens}"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-message-token-toggle']")
                .unwrap()
                .is_none(),
            "no expand affordance when there are no hidden scopes"
        );
        assert_eq!(detail_count(&container), 0, "no detail lines exist");
    }

    /// A fully-unavailable usage (backend reported nothing) renders no token
    /// block at all — never a fake-zero figure.
    #[wasm_bindgen_test]
    async fn mobile_chat_unavailable_renders_no_fake_zero() {
        let entry = assistant_entry(Some(MessageTokenUsage::unavailable(
            TokenUsageUnavailableReason::BackendDidNotReport,
        )));
        let container = mount(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='chat-message-tokens']")
                .unwrap()
                .is_none(),
            "an all-unavailable usage must not render a token block"
        );
        let body = container.text_content().unwrap_or_default();
        assert!(
            !body.contains("in:0 out:0"),
            "must not show a fake-zero token figure: {body}"
        );
    }
}
