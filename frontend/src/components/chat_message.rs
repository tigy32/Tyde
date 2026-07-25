use leptos::prelude::*;
use protocol::MessageSender;
use wasm_bindgen::JsCast;

use crate::components::tool_card::ToolCardListView;
use crate::markdown::{render_markdown, top_level_block_boundaries};
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
    /// Where attached images belong. Kept as a slot rather than appended by the
    /// renderer so the legacy order — content, images, tools — survives, and so
    /// the position is decided in one testable place instead of falling out of
    /// the order of blocks in the view.
    Images,
}

/// Rebuild a message as interleaved content, tool and image segments.
///
/// - Tools are placed by the `content_offset` recorded on the matching
///   [`protocol::ToolUseData`], matched by id.
/// - Offsets are Unicode scalar indices, so they are resolved through
///   `char_indices` and can never land inside a character.
/// - `allowed_splits` are byte offsets a split may fall on. For Markdown these
///   are top-level block boundaries, so every fragment parses exactly as it
///   does inside the whole document; a tool observed inside a code fence, link
///   or list is placed before that block rather than tearing it in half. Pass
///   `None` for content that is not parsed as Markdown, where any scalar
///   boundary is safe.
/// - A tool with no offset — legacy data, or a backend that does not record one
///   — keeps the old layout and is emitted after all content **and after the
///   images**, which is exactly where it rendered before.
/// - An offset past the end clamps to the end; a tool whose id matches no call
///   is treated as offsetless. Bad metadata degrades to the legacy layout, it
///   does not crash the chat.
/// - Several tools at one placement keep their arrival order, and the sort is
///   stable, so equal or equal-after-snapping offsets are not a reordering
///   hazard.
pub(crate) fn interleave_message(
    content: &str,
    tool_calls: &[protocol::ToolUseData],
    tools: Vec<ToolRequestEntry>,
    allowed_splits: Option<&[usize]>,
    has_images: bool,
) -> Vec<MessageSegment> {
    // Scalar index -> byte index. Everything below works in bytes.
    let byte_at = |scalar: usize| -> usize {
        content
            .char_indices()
            .nth(scalar)
            .map(|(byte, _)| byte)
            .unwrap_or(content.len())
    };
    // Snap down to the nearest position a split may legally fall on.
    let snap = |byte: usize| -> usize {
        match allowed_splits {
            Some(allowed) => allowed
                .iter()
                .copied()
                .filter(|candidate| *candidate <= byte)
                .next_back()
                .unwrap_or(0),
            None => byte,
        }
    };

    let placement_for = |tool_call_id: &str| -> Option<usize> {
        tool_calls
            .iter()
            .find(|call| call.id == tool_call_id)
            .and_then(|call| call.content_offset)
            .map(|offset| snap(byte_at(offset as usize)))
    };

    let mut placed: Vec<(usize, ToolRequestEntry)> = Vec::new();
    let mut trailing: Vec<ToolRequestEntry> = Vec::new();
    for entry in tools {
        match placement_for(&entry.request.tool_call_id) {
            Some(byte) => placed.push((byte, entry)),
            None => trailing.push(entry),
        }
    }

    let mut segments = Vec::new();
    let mut push_content = |segments: &mut Vec<MessageSegment>, slice: &str| {
        if !slice.is_empty() {
            segments.push(MessageSegment::Content(slice.to_owned()));
        }
    };

    if placed.is_empty() {
        // Legacy layout, unchanged: content, images, tools.
        push_content(&mut segments, content);
    } else {
        // Stable, so tools sharing a placement keep arrival order.
        placed.sort_by_key(|(byte, _)| *byte);
        let mut cursor = 0usize;
        let mut index = 0usize;
        while index < placed.len() {
            let byte = placed[index].0;
            if byte > cursor {
                push_content(&mut segments, &content[cursor..byte]);
                cursor = byte;
            }
            let mut group = Vec::new();
            while index < placed.len() && placed[index].0 == byte {
                group.push(placed[index].1.clone());
                index += 1;
            }
            segments.push(MessageSegment::Tools(group));
        }
        push_content(&mut segments, &content[cursor..]);
    }

    if has_images {
        segments.push(MessageSegment::Images);
    }
    if !trailing.is_empty() {
        segments.push(MessageSegment::Tools(trailing));
    }
    segments
}

/// Attached images, rendered in the slot [`interleave_message`] chose for them.
fn render_message_images(images: Vec<protocol::ImageData>) -> impl IntoView {
    view! {
        <div class="chat-card-images">
            {images.into_iter().map(|img| {
                let src = format!("data:{};base64,{}", img.media_type, img.data);
                let href = matches!(
                    img.media_type.as_str(),
                    "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp"
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
    }
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
                // Third member is `is_user`; the first is the card's CSS class.
                let is_user = card_meta.with(|(_, _, is_user, _, _)| *is_user);
                // Assistant content is Markdown, so splits may only fall on
                // top-level block boundaries — an offset inside a code fence,
                // link or list would otherwise tear the document in half and
                // each half would parse as something else. User content is
                // escaped text, where any scalar boundary is safe.
                let allowed_splits = (!is_user)
                    .then(|| top_level_block_boundaries(&entry.message.content));
                let images = entry.message.images.clone();
                let segments = interleave_message(
                    &entry.message.content,
                    &entry.message.tool_calls,
                    entry.tool_requests,
                    allowed_splits.as_deref(),
                    images.as_ref().is_some_and(|imgs| !imgs.is_empty()),
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
                        MessageSegment::Images => {
                            let Some(imgs) = images.clone() else {
                                return ().into_any();
                            };
                            render_message_images(imgs).into_any()
                        }
                    })
                    .collect::<Vec<_>>()
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
                MessageSegment::Images => "images".to_owned(),
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
            None,
            false,
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
        let segments = interleave_message(
            content,
            &[tool_call("t1", Some(6))],
            vec![tool_entry("t1")],
            None,
            false,
        );

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
            None,
            false,
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
            None,
            false,
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
            None,
            false,
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
            None,
            false,
        );
        assert_eq!(
            segment_shape(&unmatched),
            vec!["content:text", "tools:t1"],
            "a tool with no matching call is placed as legacy data"
        );

        let zero = interleave_message("text", &[tool_call("t1", Some(0))], vec![tool_entry("t1")],
            None,
            false,
        );
        assert_eq!(
            segment_shape(&zero),
            vec!["tools:t1", "content:text"],
            "offset zero puts the tool before all content, with no empty run"
        );

        let empty_content =
            interleave_message("", &[tool_call("t1", Some(0))], vec![tool_entry("t1")],
            None,
            false,
        );
        assert_eq!(
            segment_shape(&empty_content),
            vec!["tools:t1"],
            "an empty message emits no empty content segment"
        );
    }

    /// RLV-02: a tool offset that lands inside a Markdown construct must not
    /// tear the document in half. Snapping to a top-level block boundary keeps
    /// each fragment a whole document, so the code fence, list and link survive.
    #[wasm_bindgen_test]
    fn markdown_constructs_are_never_split_mid_construct() {
        // Offset 20 falls inside the fenced code block.
        let content = "intro\n\n```rust\nfn a() {}\n```\n\ntail";
        let boundaries = top_level_block_boundaries(content);
        let segments = interleave_message(
            content,
            &[tool_call("t1", Some(20))],
            vec![tool_entry("t1")],
            Some(&boundaries),
            false,
        );

        let shape = segment_shape(&segments);
        let rejoined: String = segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::Content(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(rejoined, content, "no content may be lost or duplicated");
        for segment in &segments {
            if let MessageSegment::Content(text) = segment {
                assert_eq!(
                    text.matches("```").count() % 2,
                    0,
                    "a fragment must never contain half a code fence: {text:?}"
                );
            }
        }
        assert!(
            shape.iter().any(|s| s.starts_with("tools:")),
            "the tool is still placed, just at a safe boundary: {shape:?}"
        );
    }

    /// The same rule for a list and an inline link: splitting inside either
    /// would leave one half without its syntax.
    #[wasm_bindgen_test]
    fn lists_and_links_survive_an_interior_offset() {
        let content = "- one\n- two\n\nsee [label](https://example.com) here";
        let boundaries = top_level_block_boundaries(content);

        // Inside the list.
        let in_list = interleave_message(
            content,
            &[tool_call("t1", Some(8))],
            vec![tool_entry("t1")],
            Some(&boundaries),
            false,
        );
        for segment in &in_list {
            if let MessageSegment::Content(text) = segment {
                let trimmed = text.trim();
                assert!(
                    trimmed.is_empty() || !trimmed.starts_with("two"),
                    "the list must not be cut between its items: {text:?}"
                );
            }
        }

        // Inside the link target.
        let in_link = interleave_message(
            content,
            &[tool_call("t1", Some(30))],
            vec![tool_entry("t1")],
            Some(&boundaries),
            false,
        );
        for segment in &in_link {
            if let MessageSegment::Content(text) = segment {
                assert_eq!(
                    text.matches('[').count(),
                    text.matches(']').count(),
                    "a fragment must not contain half a link: {text:?}"
                );
            }
        }
    }

    /// RLV-03: offsetless tools keep the legacy order exactly — content, then
    /// images, then tools. Appending images after the segments silently moved
    /// every legacy multimodal message to content → tools → images.
    #[wasm_bindgen_test]
    fn images_keep_their_legacy_slot_for_offsetless_tools() {
        let segments = interleave_message(
            "text",
            &[tool_call("t1", None)],
            vec![tool_entry("t1")],
            None,
            true,
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:text", "images", "tools:t1"],
            "legacy order is content, images, tools"
        );
    }

    /// Mixed placed and unplaced: images have no offset, so they keep the slot
    /// before the legacy tail rather than being pushed to the end.
    #[wasm_bindgen_test]
    fn images_sit_before_the_legacy_tail_in_a_mixed_message() {
        let segments = interleave_message(
            "AB",
            &[tool_call("placed", Some(1)), tool_call("legacy", None)],
            vec![tool_entry("placed"), tool_entry("legacy")],
            None,
            true,
        );

        assert_eq!(
            segment_shape(&segments),
            vec![
                "content:A",
                "tools:placed",
                "content:B",
                "images",
                "tools:legacy"
            ]
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
            None,
            false,
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

    /// DOM reading order for the legacy shape: content, image, tool card. This
    /// is what assistive technology consumes, and appending images after the
    /// segments silently reversed the last two.
    #[wasm_bindgen_test]
    async fn offsetless_tool_keeps_image_before_the_card_in_dom_order() {
        let mut entry = assistant_msg(None);
        entry.message.content = "some text".to_owned();
        entry.message.tool_calls = vec![tool_call("t1", None)];
        entry.message.images = Some(vec![protocol::ImageData {
            media_type: "image/png".to_owned(),
            data: "iVBORw0KGgo=".to_owned(),
        }]);
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        let order = dom_order(&container);
        let body = order
            .iter()
            .position(|kind| kind == "body")
            .expect("content renders");
        let images = order
            .iter()
            .position(|kind| kind == "images")
            .expect("image renders");
        let tools = order
            .iter()
            .position(|kind| kind == "tools")
            .expect("tool card renders");
        assert!(
            body < images && images < tools,
            "legacy reading order is content, image, tool card; got {order:?}"
        );
        assert_eq!(
            container
                .query_selector(".chat-card-image-link")
                .unwrap()
                .and_then(|el| el.get_attribute("aria-label"))
                .as_deref(),
            Some("Open image full size"),
            "the image keeps its accessible name in the legacy slot"
        );
    }

    /// A placed tool renders between the two content runs in the DOM, which is
    /// the reading order the live run got wrong.
    #[wasm_bindgen_test]
    async fn placed_tool_renders_between_content_runs_in_dom_order() {
        let mut entry = assistant_msg(None);
        entry.message.content = "PRE\n\nPOST".to_owned();
        // Offset 5 is the start of the second paragraph.
        entry.message.tool_calls = vec![tool_call("t1", Some(5))];
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        let order = dom_order(&container);
        let tools = order
            .iter()
            .position(|kind| kind == "tools")
            .expect("tool card renders");
        let bodies: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, kind)| *kind == "body")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(bodies.len(), 2, "content is split in two, got {order:?}");
        assert!(
            bodies[0] < tools && tools < bodies[1],
            "the card sits between the runs, got {order:?}"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("PRE") && text.contains("POST"),
            "no content is lost, got: {text}"
        );
    }

    /// A tool offset inside a fenced code block must leave the block intact:
    /// one `<pre>`/code block, its full text, and no stray literal fence.
    #[wasm_bindgen_test]
    async fn tool_inside_a_code_fence_leaves_the_block_intact() {
        let mut entry = assistant_msg(None);
        entry.message.content = "intro\n\n```rust\nfn a() {}\n```\n\ntail".to_owned();
        entry.message.tool_calls = vec![tool_call("t1", Some(20))];
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        assert_eq!(
            container.query_selector_all(".md-code-block").unwrap().length(),
            1,
            "the fenced block renders once and whole"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("fn a() {}"),
            "the code survives the split, got: {text}"
        );
        assert!(
            !text.contains("```"),
            "no half-fence leaks through as literal text, got: {text}"
        );
    }

    /// A tool offset inside a link must not leave half the link as literal
    /// text: the anchor still renders with its href and label.
    #[wasm_bindgen_test]
    async fn tool_inside_a_link_leaves_the_anchor_intact() {
        let mut entry = assistant_msg(None);
        entry.message.content = "see [label](https://example.com) here".to_owned();
        entry.message.tool_calls = vec![tool_call("t1", Some(12))];
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        let anchor = container
            .query_selector(".chat-card-body a")
            .unwrap()
            .expect("the link still renders as an anchor");
        assert_eq!(
            anchor.get_attribute("href").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(anchor.text_content().as_deref(), Some("label"));
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("](https://"),
            "no half-link leaks through as literal text, got: {text}"
        );
    }

    /// A tool offset inside a list must leave both items in one list.
    #[wasm_bindgen_test]
    async fn tool_inside_a_list_leaves_both_items_in_one_list() {
        let mut entry = assistant_msg(None);
        entry.message.content = "- one\n- two\n\ntail".to_owned();
        entry.message.tool_calls = vec![tool_call("t1", Some(8))];
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        let lists = container.query_selector_all(".chat-card-body ul").unwrap();
        assert_eq!(lists.length(), 1, "the list is not cut in two");
        let items = container.query_selector_all(".chat-card-body li").unwrap();
        assert_eq!(items.length(), 2, "both items stay in it");
    }

    /// Reading order of the rendered card, as a screen reader would walk it.
    fn dom_order(container: &HtmlElement) -> Vec<String> {
        let nodes = container
            .query_selector_all(".chat-card-body, .chat-card-images, .chat-card-tools")
            .unwrap();
        (0..nodes.length())
            .filter_map(|index| nodes.item(index))
            .filter_map(|node| node.dyn_into::<web_sys::Element>().ok())
            .map(|el| {
                let class = el.get_attribute("class").unwrap_or_default();
                if class.contains("chat-card-images") {
                    "images".to_owned()
                } else if class.contains("chat-card-body") {
                    "body".to_owned()
                } else {
                    "tools".to_owned()
                }
            })
            .collect()
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
