use leptos::prelude::*;
use protocol::MessageSender;
use wasm_bindgen::JsCast;

use crate::components::tool_card::ToolCardListView;
use crate::markdown::render_markdown;
use crate::state::{ActiveAgentRef, ChatRowHandle};

/// Render a single chat row from its row-local signal.
///
/// `ChatView` keys rows by stable `ChatRowId` and passes the row handle into
/// this component. Appending a sibling row updates the row list, but existing
/// `ChatMessageView`s only subscribe to their own `ArcRwSignal`, so long
/// history replay does not wake every already-mounted row.
#[component]
pub fn ChatMessageView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    row: ChatRowHandle,
) -> impl IntoView {
    let entry = row.entry;

    // Each Memo reads through `with` to avoid cloning the entire
    // ChatMessageEntry (which carries a potentially-long
    // `message.content: String`) just to extract a field. Memos
    // already dedup via `PartialEq` on the projected tuple, so this
    // is purely savings on the per-evaluation alloc cost.
    let entry_for_meta = entry.clone();
    let card_meta: Memo<(String, String, bool, bool)> = Memo::new(move |_| {
        entry_for_meta.with(|e| match &e.message.sender {
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
        })
    });

    let entry_for_content = entry.clone();
    let content_data: Memo<(bool, String)> = Memo::new(move |_| {
        entry_for_content.with(|e| {
            let is_user = matches!(e.message.sender, MessageSender::User);
            (is_user, e.message.content.clone())
        })
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

    let entry_for_timestamp = entry.clone();
    let timestamp_memo: Memo<u64> =
        Memo::new(move |_| entry_for_timestamp.with(|e| e.message.timestamp));

    let entry_for_model = entry.clone();
    let model_memo: Memo<Option<String>> = Memo::new(move |_| {
        entry_for_model.with(|e| e.message.model_info.as_ref().map(|mi| mi.model.clone()))
    });

    let copy_state = RwSignal::new("copy");

    let entry_for_copy = entry.clone();
    let on_copy = move |_| {
        let text = entry_for_copy.with_untracked(|entry| entry.message.content.clone());
        if text.is_empty() {
            return;
        }
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
    let reasoning_open = RwSignal::new(false);

    let entry_for_reasoning = entry.clone();
    let entry_for_images = entry.clone();
    let entry_for_tools = entry.clone();
    let entry_for_footer = entry.clone();
    let entry_for_reasoning_slot = StoredValue::new_local(entry_for_reasoning.clone());

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
                entry_for_reasoning.with(|entry| entry.message.reasoning.as_ref().map(|r| r.tokens)).map(|token_count| {
                    view! {
                        <details
                            class="chat-card-reasoning"
                            on:toggle=move |ev: leptos::ev::Event| {
                                if let Some(target) = ev.target()
                                    && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>()
                                {
                                    reasoning_open.set(el.open());
                                }
                            }
                        >
                            <summary>
                                <span class="reasoning-icon">"💭"</span>
                                " Thinking"
                                {token_count.map(|t| view! {
                                    <span class="reasoning-tokens">{format!(" ({} tokens)", format_compact(t))}</span>
                                })}
                            </summary>
                            <Show when=move || reasoning_open.get()>
                                {move || {
                                    entry_for_reasoning_slot.with_value(|entry_for_reasoning_body| entry_for_reasoning_body.with(|entry| {
                                        entry.message.reasoning.as_ref().map(|reasoning| {
                                            view! {
                                                <pre class="reasoning-content">{reasoning.text.clone()}</pre>
                                            }
                                        })
                                    }))
                                }}
                            </Show>
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
                entry_for_images.get().message.images.and_then(|imgs| {
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

            // Tool cards — read only this row's signal, so tool updates do not
            // invalidate sibling rows.
            {move || {
                let tools = entry_for_tools.get().tool_requests;
                if tools.is_empty() {
                    return None;
                }
                Some(view! {
                    <ToolCardListView agent_ref=agent_ref entries=tools />
                })
            }}

            // Footer (assistant only)
            {move || {
                let is_assistant = card_meta.with(|(_, _, _, ia)| *ia);
                if !is_assistant {
                    return None;
                }
                let e = entry_for_footer.get();
                let model_display = e.message.model_info.as_ref().map(|mi| mi.model.clone());
                let agent_display = match &e.message.sender {
                    MessageSender::Assistant { agent } => agent.clone(),
                    _ => String::new(),
                };
                let badge = this_turn_token_usage(&e.message)
                    .as_ref()
                    .map(token_badge_data);
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

/// Resolve the THIS-TURN token usage for a chat row. `token_usage` is the
/// authoritative this-turn figure; `turn_token_usage` refines it: `Known`
/// carries the same this-turn figure explicitly (never the cumulative
/// `agent_total`), and `Unavailable` means the backend reported nothing, so
/// the row renders no badge rather than a fake-zero one.
pub(crate) fn this_turn_token_usage(
    message: &protocol::ChatMessage,
) -> Option<protocol::TokenUsage> {
    match &message.turn_token_usage {
        Some(protocol::TurnTokenUsage::Unavailable { .. }) => None,
        Some(protocol::TurnTokenUsage::Known { this_turn, .. }) => Some((**this_turn).clone()),
        None => message.token_usage.clone(),
    }
}

/// Format a `TokenUsage` into `(input_text, output_text, tooltip)` for the
/// token badge: `↑input (cached N)` / `↓output (reasoning N)`. Shared so other
/// surfaces (e.g. the agent-control await stats line) render tokens identically.
pub(crate) fn token_badge_data(tu: &protocol::TokenUsage) -> (String, String, String) {
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

pub(crate) fn format_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, ChatMessageEntry, ChatRowHandle};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, ChatMessage, ChatMessageId, MessageMetadataUpdateData, TokenUsage,
        TokenUsageUnavailableReason, TurnTokenUsage,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 800px; height: 600px; \
                 z-index: 2147483647; background: white; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
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

    fn assistant_msg(
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

    fn mount_message(entry: ChatMessageEntry) -> HtmlElement {
        let container = make_container();
        // Leak the mount handle so the component stays mounted after this
        // helper returns; dropping it would unmount and clear the container.
        mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            let agent_ref: Signal<Option<crate::state::ActiveAgentRef>> =
                RwSignal::new(None).into();
            let row = ChatRowHandle::new(entry.clone());
            view! { <ChatMessageView agent_ref=agent_ref row=row /> }
        })
        .forget();
        container
    }

    fn input_stat(container: &HtmlElement) -> Option<String> {
        container
            .query_selector(".token-stat-input")
            .unwrap()
            .map(|el| el.text_content().unwrap_or_default())
    }

    fn output_stat(container: &HtmlElement) -> Option<String> {
        container
            .query_selector(".token-stat-output")
            .unwrap()
            .map(|el| el.text_content().unwrap_or_default())
    }

    /// The chat row's token badge shows the THIS-TURN figure, never the
    /// cumulative `agent_total` carried alongside it in `TurnTokenUsage::Known`.
    #[wasm_bindgen_test]
    async fn chat_row_shows_this_turn_not_cumulative_total() {
        // This turn is small; the agent's cumulative total is huge and distinct.
        let this_turn = usage(1200, 300);
        let agent_total = usage(999_000, 888_000);
        let entry = assistant_msg(
            Some(this_turn.clone()),
            Some(TurnTokenUsage::Known {
                this_turn: Box::new(this_turn),
                agent_total: Box::new(agent_total),
            }),
        );
        let container = mount_message(entry);
        next_tick().await;

        let input = input_stat(&container).expect("input token stat present");
        let output = output_stat(&container).expect("output token stat present");
        assert!(
            input.contains("1.2K"),
            "row must show the this-turn input figure: {input}"
        );
        assert!(
            output.contains("300"),
            "row must show the this-turn output figure: {output}"
        );
        // The large cumulative total must not leak into the per-turn badge.
        assert!(
            !input.contains("999") && !output.contains("888"),
            "row must not render the cumulative agent_total: in={input} out={output}"
        );
    }

    /// `TurnTokenUsage::Unavailable` means the backend reported nothing this
    /// turn; the row must render no token badge rather than a fake-zero one,
    /// even though `token_usage` carries zeros for backward compatibility.
    #[wasm_bindgen_test]
    async fn chat_row_unavailable_renders_no_fake_zero_badge() {
        let entry = assistant_msg(
            Some(usage(0, 0)),
            Some(TurnTokenUsage::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            }),
        );
        let container = mount_message(entry);
        next_tick().await;

        assert!(
            input_stat(&container).is_none(),
            "Unavailable turn must not render an input token stat"
        );
        assert!(
            output_stat(&container).is_none(),
            "Unavailable turn must not render an output token stat"
        );
        let body = container.text_content().unwrap_or_default();
        assert!(
            !body.contains("\u{2191}0") && !body.contains("\u{2193}0"),
            "Unavailable turn must not show a fake-zero token badge: {body}"
        );
    }

    /// A live `MessageMetadataUpdated` patch that flips a row's
    /// `turn_token_usage` from `Unavailable` to `Known` must reactively update
    /// the mounted row to show the real this-turn figure — no badge before,
    /// the real numbers after. This exercises both the reactive projection and
    /// the live patch reducer (`apply_chat_message_metadata`).
    #[wasm_bindgen_test]
    async fn chat_row_live_patch_unavailable_to_known_updates_badge() {
        let container = make_container();
        let agent_id = AgentId("a-live-patch".to_owned());
        let message_id = ChatMessageId("msg-live".to_owned());

        // Stash the state created inside the reactive owner so the test body
        // can drive the live patch after mounting.
        let shared: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let shared_for_mount = shared.clone();
        let agent_id_mount = agent_id.clone();
        let message_id_mount = message_id.clone();

        mount_to(container.clone(), move || {
            let state = AppState::new();
            let entry = ChatMessageEntry {
                message: ChatMessage {
                    message_id: Some(message_id_mount.clone()),
                    timestamp: 0,
                    sender: protocol::MessageSender::Assistant {
                        agent: "codex".to_owned(),
                    },
                    content: "hello".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: Some(usage(0, 0)),
                    turn_token_usage: Some(TurnTokenUsage::Unavailable {
                        reason: TokenUsageUnavailableReason::BackendDidNotReport,
                    }),
                    context_breakdown: None,
                    images: None,
                },
                tool_requests: Vec::new(),
            };
            let row = state.push_chat_entry(agent_id_mount.clone(), entry);
            *shared_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref: Signal<Option<crate::state::ActiveAgentRef>> =
                RwSignal::new(None).into();
            view! { <ChatMessageView agent_ref=agent_ref row=row /> }
        })
        .forget();
        next_tick().await;

        // Before the patch the turn is Unavailable: no badge at all.
        assert!(
            input_stat(&container).is_none(),
            "Unavailable turn renders no input stat before the patch"
        );
        assert!(
            output_stat(&container).is_none(),
            "Unavailable turn renders no output stat before the patch"
        );

        // Live patch: the backend reports the turn's real usage.
        let state = shared.borrow().clone().expect("state captured at mount");
        state.apply_chat_message_metadata(
            &agent_id,
            MessageMetadataUpdateData {
                message_id: message_id.clone(),
                model_info: None,
                token_usage: Some(usage(0, 0)),
                turn_token_usage: Some(TurnTokenUsage::Known {
                    this_turn: Box::new(usage(4200, 1300)),
                    agent_total: Box::new(usage(50_000, 20_000)),
                }),
                context_breakdown: None,
            },
        );
        next_tick().await;

        let input = input_stat(&container).expect("badge appears after the live patch");
        let output = output_stat(&container).expect("output stat appears after the live patch");
        assert!(
            input.contains("4.2K"),
            "row updates to the real this-turn input figure: {input}"
        );
        assert!(
            output.contains("1.3K"),
            "row updates to the real this-turn output figure: {output}"
        );
        // The cumulative agent_total must never leak into the per-turn badge.
        assert!(
            !input.contains("50") && !output.contains("20"),
            "cumulative agent_total must not leak: in={input} out={output}"
        );
    }
}
