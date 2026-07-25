use leptos::prelude::*;
use protocol::MessageSender;
use wasm_bindgen::JsCast;

use crate::components::tool_card::ToolCardListView;
use crate::markdown::render_markdown;
use crate::state::{ActiveAgentRef, ChatRowHandle, ToolRequestEntry};

/// Render a single chat row from its row-local signal.
///
/// `ChatView` keys rows by stable `ChatRowId` and passes the row handle into
/// this component. Appending a sibling row updates the row list, but existing
/// `ChatMessageView`s only subscribe to their own `ArcRwSignal`, so long
/// history replay does not wake every already-mounted row.
/// One rendered slice of an assistant message.
///
/// A message carries a single content string plus a positionless tool list, so
/// rendering used to be "all text, then all tools" — which loses the order the
/// model actually produced when it interleaves prose and tool calls. Each tool
/// now records the scalar offset in `content` at which it was observed, and
/// the message is rebuilt as the alternating sequence that implies.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MessageSegment {
    Content(String),
    Tools(Vec<ToolRequestEntry>),
}

/// Rebuild a message as interleaved content and tool segments.
///
/// - Tools are placed by the `content_offset` recorded on the matching
///   [`protocol::ToolUseData`], matched by id.
/// - A tool with no offset — legacy data, or a backend that does not record one
///   — keeps the old layout and is emitted after all content. If *no* tool has
///   an offset the output is exactly `[Content(all), Tools(all)]`, byte for
///   byte what this rendered before.
/// - Offsets are Unicode scalar indices, so splitting goes through
///   `char_indices` and can never land inside a character.
/// - An offset past the end clamps to the end rather than panicking; a
///   backend that miscounts degrades to the legacy tail, it does not crash the
///   chat.
/// - Several tools at one offset keep their arrival order, and equal offsets
///   sort stably, so a repeated offset is not a reordering hazard.
pub(crate) fn interleave_message(
    content: &str,
    tool_calls: &[protocol::ToolUseData],
    tools: Vec<ToolRequestEntry>,
) -> Vec<MessageSegment> {
    let scalar_len = content.chars().count();
    let offset_for = |tool_call_id: &str| -> Option<usize> {
        tool_calls
            .iter()
            .find(|call| call.id == tool_call_id)
            .and_then(|call| call.content_offset)
            .map(|offset| (offset as usize).min(scalar_len))
    };

    // Partition into placed and unplaced, preserving arrival order in both.
    let mut placed: Vec<(usize, ToolRequestEntry)> = Vec::new();
    let mut trailing: Vec<ToolRequestEntry> = Vec::new();
    for entry in tools {
        match offset_for(&entry.request.tool_call_id) {
            Some(offset) => placed.push((offset, entry)),
            None => trailing.push(entry),
        }
    }

    if placed.is_empty() {
        // Legacy layout, unchanged.
        let mut segments = Vec::new();
        if !content.is_empty() {
            segments.push(MessageSegment::Content(content.to_owned()));
        }
        if !trailing.is_empty() {
            segments.push(MessageSegment::Tools(trailing));
        }
        return segments;
    }

    // `sort_by_key` is stable, so tools sharing an offset keep arrival order.
    placed.sort_by_key(|(offset, _)| *offset);

    // Scalar index -> byte index, so a split can never fall inside a character.
    let byte_at = |scalar: usize| -> usize {
        content
            .char_indices()
            .nth(scalar)
            .map(|(byte, _)| byte)
            .unwrap_or(content.len())
    };

    let mut segments = Vec::new();
    let mut cursor_scalar = 0usize;
    let mut index = 0usize;
    while index < placed.len() {
        let offset = placed[index].0;
        if offset > cursor_scalar {
            let slice = &content[byte_at(cursor_scalar)..byte_at(offset)];
            if !slice.is_empty() {
                segments.push(MessageSegment::Content(slice.to_owned()));
            }
            cursor_scalar = offset;
        }
        // Every tool at this offset becomes one group.
        let mut group = Vec::new();
        while index < placed.len() && placed[index].0 == offset {
            group.push(placed[index].1.clone());
            index += 1;
        }
        segments.push(MessageSegment::Tools(group));
    }

    let tail = &content[byte_at(cursor_scalar)..];
    if !tail.is_empty() {
        segments.push(MessageSegment::Content(tail.to_owned()));
    }
    if !trailing.is_empty() {
        segments.push(MessageSegment::Tools(trailing));
    }
    segments
}

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
    let card_meta: Memo<(String, String, bool, bool, bool)> = Memo::new(move |_| {
        entry_for_meta.with(|e| match &e.message.sender {
            MessageSender::User => (
                "chat-card chat-card-user".to_owned(),
                "You".to_owned(),
                true,
                false,
                false,
            ),
            MessageSender::Assistant { agent } => (
                "chat-card chat-card-assistant".to_owned(),
                agent.clone(),
                false,
                true,
                false,
            ),
            MessageSender::System => (
                "chat-card chat-card-system".to_owned(),
                "System".to_owned(),
                false,
                false,
                false,
            ),
            MessageSender::Warning => (
                "chat-card chat-card-warning".to_owned(),
                "Warning".to_owned(),
                false,
                false,
                false,
            ),
            MessageSender::Error => (
                "chat-card chat-card-error".to_owned(),
                "Error".to_owned(),
                false,
                false,
                true,
            ),
        })
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
        <div
            class=move || card_meta.with(|(c, _, _, _, _)| c.clone())
            role=move || card_meta.with(|(_, _, _, _, is_error)| is_error.then_some("alert"))
            aria-label=move || card_meta.with(|(_, _, _, _, is_error)| is_error.then_some("Error message"))
        >
            <div class="chat-card-header">
                <span class="chat-card-sender">{move || card_meta.with(|(_, s, _, _, _)| s.clone())}</span>
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

            // Body and tool cards, interleaved in the order the model produced
            // them. Reads only this row's signal, so a tool update does not
            // invalidate sibling rows.
            {move || {
                let entry = entry_for_tools.get();
                let is_user = card_meta.with(|(is_user, _, _, _, _)| *is_user);
                let segments = interleave_message(
                    &entry.message.content,
                    &entry.message.tool_calls,
                    entry.tool_requests,
                );
                let mut body_ref_taken = false;
                segments
                    .into_iter()
                    .map(|segment| match segment {
                        MessageSegment::Content(text) => {
                            let html = if is_user {
                                let escaped = text
                                    .replace('&', "&amp;")
                                    .replace('<', "&lt;")
                                    .replace('>', "&gt;");
                                format!("<span class=\"user-text\">{escaped}</span>")
                            } else {
                                render_markdown(&text)
                            };
                            if html.is_empty() {
                                return ().into_any();
                            }
                            // The node ref identifies the row's body; with the
                            // body split it belongs to the first slice.
                            let node_ref = (!body_ref_taken).then(|| {
                                body_ref_taken = true;
                                body_ref
                            });
                            match node_ref {
                                Some(node_ref) => view! {
                                    <div
                                        class="chat-card-body"
                                        node_ref=node_ref
                                        inner_html=html
                                    ></div>
                                }
                                .into_any(),
                                None => view! {
                                    <div class="chat-card-body" inner_html=html></div>
                                }
                                .into_any(),
                            }
                        }
                        MessageSegment::Tools(entries) => view! {
                            <ToolCardListView agent_ref=agent_ref entries=entries />
                        }
                        .into_any(),
                    })
                    .collect::<Vec<_>>()
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
                                let href = matches!(
                                    img.media_type.as_str(),
                                    "image/png"
                                        | "image/jpeg"
                                        | "image/jpg"
                                        | "image/gif"
                                        | "image/webp"
                                )
                                .then(|| src.clone());
                                view! {
                                    <a
                                        class="chat-card-image-link"
                                        href=href
                                        target="_blank"
                                        rel="noopener"
                                        aria-label="Open image full size"
                                    >
                                        <img class="chat-card-image" src=src alt="Chat image" loading="lazy" />
                                    </a>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    })
                })
            }}


            // Footer (assistant only)
            {move || {
                let is_assistant = card_meta.with(|(_, _, _, ia, _)| *ia);
                if !is_assistant {
                    return None;
                }
                let e = entry_for_footer.get();
                let model_display = e.message.model_info.as_ref().map(|mi| mi.model.clone());
                let agent_display = match &e.message.sender {
                    MessageSender::Assistant { agent } => agent.clone(),
                    _ => String::new(),
                };
                // The footer shows the per-request figure by default; the
                // tooltip lays out all three scopes (request/turn/cumulative)
                // so the inline number is never ambiguous.
                let usage = e.message.token_usage.clone();
                let badge = request_token_usage(&e.message)
                    .as_ref()
                    .map(token_badge_data);
                let badge_tooltip = usage
                    .as_ref()
                    .map(message_token_tooltip)
                    .unwrap_or_default();
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

/// The per-request token usage a chat row shows by default: `token_usage.request`
/// when the backend reported it, else `None` (no fake-zero badge). The turn and
/// cumulative scopes are surfaced in the badge tooltip via
/// [`message_token_tooltip`], never folded into this figure.
pub(crate) fn request_token_usage(message: &protocol::ChatMessage) -> Option<protocol::TokenUsage> {
    message
        .token_usage
        .as_ref()
        .and_then(|usage| usage.request.known_usage().cloned())
}

fn token_usage_unavailable_reason_text(
    reason: protocol::TokenUsageUnavailableReason,
) -> &'static str {
    match reason {
        protocol::TokenUsageUnavailableReason::BackendDidNotReport => "backend did not report",
        protocol::TokenUsageUnavailableReason::ProviderScopeAmbiguous => "provider scope ambiguous",
    }
}

/// One scope's line for the token tooltip: the `↑input ↓output` figure when
/// known, else an explicit "unavailable" note with the server-provided reason —
/// never a fabricated zero.
fn token_scope_summary(scope: &protocol::TokenUsageScope) -> String {
    match scope {
        protocol::TokenUsageScope::Known { usage } => {
            let (input_text, output_text, _) = token_badge_data(usage);
            format!("{input_text} {output_text}")
        }
        protocol::TokenUsageScope::Unavailable { reason } => {
            format!(
                "unavailable ({})",
                token_usage_unavailable_reason_text(*reason)
            )
        }
    }
}

/// Multi-scope tooltip laying out request / turn / cumulative usage so the row's
/// inline (request) number is unambiguous and the turn + cumulative scopes are
/// exposed on hover.
pub(crate) fn message_token_tooltip(usage: &protocol::MessageTokenUsage) -> String {
    format!(
        "Request: {}\nTurn: {}\nCumulative: {}",
        token_scope_summary(&usage.request),
        token_scope_summary(&usage.turn),
        token_scope_summary(&usage.cumulative),
    )
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
        AgentId, ChatMessage, ChatMessageId, MessageMetadataUpdateData, MessageTokenUsage,
        TokenUsage, TokenUsageUnavailableReason,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn tool_call(id: &str, offset: Option<u32>) -> protocol::ToolUseData {
        protocol::ToolUseData {
            id: id.to_owned(),
            name: "run".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: offset,
        }
    }

    fn tool_entry(id: &str) -> ToolRequestEntry {
        ToolRequestEntry {
            request: protocol::ToolRequest {
                tool_call_id: id.to_owned(),
                tool_name: "run".to_owned(),
                tool_type: protocol::ToolRequestType::Other {
                    args: serde_json::json!({}),
                },
            },
            result: None,
        }
    }

    fn segment_shape(segments: &[MessageSegment]) -> Vec<String> {
        segments
            .iter()
            .map(|segment| match segment {
                MessageSegment::Content(text) => format!("content:{text}"),
                MessageSegment::Tools(entries) => format!(
                    "tools:{}",
                    entries
                        .iter()
                        .map(|entry| entry.request.tool_call_id.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            })
            .collect()
    }

    /// The live B1 shape: text, a tool call, then more text. Tyde rendered
    /// `PRE`, `POST`, then the tool card, because a message carries one content
    /// string and a positionless tool list. The recorded offset restores the
    /// order the model actually produced.
    #[wasm_bindgen_test]
    fn content_and_tools_interleave_at_the_recorded_offset() {
        let segments = interleave_message(
            "PRE\nPOST",
            &[tool_call("t1", Some(4))],
            vec![tool_entry("t1")],
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:PRE\n", "tools:t1", "content:POST"],
            "the tool card belongs between the two text runs, not after both"
        );
    }

    /// Offsets are Unicode scalar indices, so a split must never land inside a
    /// character. Byte indexing here would panic or corrupt the text.
    #[wasm_bindgen_test]
    fn offsets_are_scalar_indices_not_byte_indices() {
        // Four scalars, ten bytes.
        let content = "héllo→ok";
        let segments = interleave_message(content, &[tool_call("t1", Some(6))], vec![tool_entry("t1")]);

        assert_eq!(
            segment_shape(&segments),
            vec!["content:héllo→", "tools:t1", "content:ok"],
            "the split falls on the scalar boundary, keeping both runs intact"
        );
    }

    /// Several tools observed at one point keep the order they arrived in, and
    /// equal offsets must not reorder anything.
    #[wasm_bindgen_test]
    fn tools_sharing_an_offset_keep_their_arrival_order() {
        let segments = interleave_message(
            "AB",
            &[
                tool_call("first", Some(1)),
                tool_call("second", Some(1)),
                tool_call("third", Some(1)),
            ],
            vec![tool_entry("first"), tool_entry("second"), tool_entry("third")],
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:A", "tools:first,second,third", "content:B"],
            "one group at the shared offset, in arrival order"
        );
    }

    /// No offsets at all is the legacy shape and must render exactly as before:
    /// all content, then all tools.
    #[wasm_bindgen_test]
    fn absent_offsets_preserve_the_legacy_layout() {
        let segments = interleave_message(
            "all of the text",
            &[tool_call("t1", None), tool_call("t2", None)],
            vec![tool_entry("t1"), tool_entry("t2")],
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:all of the text", "tools:t1,t2"],
            "unchanged from the positionless rendering"
        );
    }

    /// Malformed or unmatched metadata must degrade, not crash: an offset past
    /// the end clamps, and a tool with no matching call keeps the legacy tail.
    #[wasm_bindgen_test]
    fn malformed_offsets_degrade_to_the_tail() {
        let past_end = interleave_message(
            "short",
            &[tool_call("t1", Some(9_999))],
            vec![tool_entry("t1")],
        );
        assert_eq!(
            segment_shape(&past_end),
            vec!["content:short", "tools:t1"],
            "an out-of-range offset clamps to the end instead of panicking"
        );

        let unmatched = interleave_message(
            "text",
            &[tool_call("other", Some(1))],
            vec![tool_entry("t1")],
        );
        assert_eq!(
            segment_shape(&unmatched),
            vec!["content:text", "tools:t1"],
            "a tool with no matching call is placed as legacy data"
        );

        let zero = interleave_message("text", &[tool_call("t1", Some(0))], vec![tool_entry("t1")]);
        assert_eq!(
            segment_shape(&zero),
            vec!["tools:t1", "content:text"],
            "offset zero puts the tool before all content, with no empty run"
        );

        let empty_content =
            interleave_message("", &[tool_call("t1", Some(0))], vec![tool_entry("t1")]);
        assert_eq!(
            segment_shape(&empty_content),
            vec!["tools:t1"],
            "an empty message emits no empty content segment"
        );
    }

    /// Placed and unplaced tools in one message: the placed one interleaves and
    /// the offsetless one keeps its legacy position after all content.
    #[wasm_bindgen_test]
    fn mixed_placed_and_unplaced_tools_each_keep_their_rule() {
        let segments = interleave_message(
            "AB",
            &[tool_call("placed", Some(1)), tool_call("legacy", None)],
            vec![tool_entry("placed"), tool_entry("legacy")],
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:A", "tools:placed", "content:B", "tools:legacy"]
        );
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

    fn assistant_msg(token_usage: Option<MessageTokenUsage>) -> ChatMessageEntry {
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

    #[wasm_bindgen_test]
    async fn supervisor_failure_warning_uses_existing_warning_card() {
        let copy = "Supervisor could not verify whether this task was complete after 2 attempts and has stopped retrying. Send a follow-up message if you want the agent to continue.";
        let container = mount_message(ChatMessageEntry {
            message: ChatMessage {
                message_id: None,
                timestamp: 0,
                sender: MessageSender::Warning,
                content: copy.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        });
        next_tick().await;

        let cards = container.query_selector_all(".chat-card-warning").unwrap();
        assert_eq!(cards.length(), 1);
        let card = container
            .query_selector(".chat-card-warning")
            .unwrap()
            .expect("warning card");
        let sender = card
            .query_selector(".chat-card-sender")
            .unwrap()
            .expect("warning sender label");
        assert_eq!(sender.text_content().as_deref(), Some("Warning"));
        let body = card
            .query_selector(".chat-card-body")
            .unwrap()
            .expect("warning body");
        let paragraph = body
            .query_selector("p")
            .unwrap()
            .expect("warning body paragraph");
        assert_eq!(paragraph.text_content().as_deref(), Some(copy));
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

    #[wasm_bindgen_test]
    async fn assistant_generated_image_renders_as_full_size_link() {
        let mut entry = assistant_msg(None);
        entry.message.content.clear();
        entry.message.images = Some(vec![protocol::ImageData {
            media_type: "image/png".to_owned(),
            data: "iVBORw0KGgo=".to_owned(),
        }]);
        let container = mount_message(entry);
        next_tick().await;

        let link = container
            .query_selector(".chat-card-image-link")
            .unwrap()
            .expect("generated image has a full-size link");
        assert_eq!(link.get_attribute("target").as_deref(), Some("_blank"));
        let image = container
            .query_selector(".chat-card-image")
            .unwrap()
            .expect("generated image renders inline");
        assert!(
            image
                .get_attribute("src")
                .is_some_and(|src| src.starts_with("data:image/png;base64,"))
        );
    }

    fn badge_title(container: &HtmlElement) -> Option<String> {
        container
            .query_selector(".token-badge")
            .unwrap()
            .and_then(|el| el.get_attribute("title"))
    }

    /// The chat row's inline badge shows the REQUEST scope by default, never the
    /// turn delta or the cumulative total carried in the same `MessageTokenUsage`.
    #[wasm_bindgen_test]
    async fn chat_row_shows_request_scope_by_default() {
        // Request is small and distinct from the larger turn / cumulative scopes.
        let entry = assistant_msg(Some(
            MessageTokenUsage::request_and_turn_known(usage(1200, 300), usage(4000, 5000))
                .with_cumulative(usage(999_000, 888_000)),
        ));
        let container = mount_message(entry);
        next_tick().await;

        let input = input_stat(&container).expect("input token stat present");
        let output = output_stat(&container).expect("output token stat present");
        assert!(
            input.contains("1.2K"),
            "row must show the request input figure: {input}"
        );
        assert!(
            output.contains("300"),
            "row must show the request output figure: {output}"
        );
        // Neither the turn delta nor the cumulative total may leak into the
        // inline badge — those live in the tooltip.
        assert!(
            !input.contains("4.0K") && !input.contains("999"),
            "turn/cumulative input must not leak into the inline badge: {input}"
        );
        assert!(
            !output.contains("5.0K") && !output.contains("888"),
            "turn/cumulative output must not leak into the inline badge: {output}"
        );
    }

    /// The badge tooltip exposes all three scopes (request / turn / cumulative)
    /// with their figures, so the inline request number is never ambiguous.
    #[wasm_bindgen_test]
    async fn chat_row_tooltip_exposes_all_three_scopes() {
        let entry = assistant_msg(Some(
            MessageTokenUsage::request_and_turn_known(usage(1200, 300), usage(4000, 5000))
                .with_cumulative(usage(999_000, 888_000)),
        ));
        let container = mount_message(entry);
        next_tick().await;

        let title = badge_title(&container).expect("token badge carries a tooltip");
        assert!(
            title.contains("Request:") && title.contains("1.2K"),
            "tooltip must label the request scope: {title}"
        );
        assert!(
            title.contains("Turn:") && title.contains("4.0K"),
            "tooltip must expose the turn scope: {title}"
        );
        assert!(
            title.contains("Cumulative:") && title.contains("999.0K"),
            "tooltip must expose the cumulative scope: {title}"
        );
    }

    /// A fully-unavailable `MessageTokenUsage` means the backend reported
    /// nothing; the row must render no token badge rather than a fake-zero one.
    #[wasm_bindgen_test]
    async fn chat_row_unavailable_renders_no_fake_zero_badge() {
        let entry = assistant_msg(Some(MessageTokenUsage::unavailable(
            TokenUsageUnavailableReason::BackendDidNotReport,
        )));
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

    /// A live `MessageMetadataUpdated` patch that flips a row's `token_usage`
    /// from unavailable to a known request scope must reactively update the
    /// mounted row to show the real request figure — no badge before, the real
    /// numbers after. This exercises both the reactive projection and the live
    /// patch reducer (`apply_chat_message_metadata`).
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
                    token_usage: Some(MessageTokenUsage::unavailable(
                        TokenUsageUnavailableReason::BackendDidNotReport,
                    )),
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

        // Before the patch the request scope is Unavailable: no badge at all.
        assert!(
            input_stat(&container).is_none(),
            "unavailable usage renders no input stat before the patch"
        );
        assert!(
            output_stat(&container).is_none(),
            "unavailable usage renders no output stat before the patch"
        );

        // Live patch: the backend reports the request's real usage, plus a
        // distinct cumulative total that must stay out of the inline badge.
        let state = shared.borrow().clone().expect("state captured at mount");
        state.apply_chat_message_metadata(
            &agent_id,
            MessageMetadataUpdateData {
                message_id: message_id.clone(),
                model_info: None,
                token_usage: Some(
                    MessageTokenUsage::request_known(usage(4200, 1300))
                        .with_cumulative(usage(50_000, 20_000)),
                ),
                context_breakdown: None,
            },
        );
        next_tick().await;

        let input = input_stat(&container).expect("badge appears after the live patch");
        let output = output_stat(&container).expect("output stat appears after the live patch");
        assert!(
            input.contains("4.2K"),
            "row updates to the real request input figure: {input}"
        );
        assert!(
            output.contains("1.3K"),
            "row updates to the real request output figure: {output}"
        );
        // The cumulative total must never leak into the inline request badge.
        assert!(
            !input.contains("50") && !output.contains("20"),
            "cumulative total must not leak into the inline badge: in={input} out={output}"
        );
    }
}
