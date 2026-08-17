use leptos::prelude::*;
use protocol::MessageSender;
use wasm_bindgen::JsCast;

use crate::components::tool_card::ToolCardListView;
use crate::markdown::render_markdown;
use crate::state::{ActiveAgentRef, ChatMessageEntry, ToolRequestEntry};

/// Render a single chat row from its row-local signal.
///
/// `ChatView` keys rows by stable `ChatRowId` and passes the row handle into
/// this component. Appending a sibling row updates the row list, but existing
/// `ChatMessageView`s only subscribe to their own `ArcRwSignal`, so long
/// history replay does not wake every already-mounted row.
/// One rendered slice of an assistant message.
///
/// Hermes wraps a whole `run_conversation` loop in one `message.start` /
/// `message.complete` pair, so several provider responses collapse into a
/// single `ChatMessage` with one content string and a positionless tool list.
/// `content_offset` comes from `tool.start`, which is an authoritative
/// **provider-response boundary**: the text before it and the text after it
/// were produced by different provider calls.
///
/// Deliberately not `PartialEq`. Comparing segments by value would require
/// `ToolRequestEntry: PartialEq`, and neither `ToolRequest` nor
/// `ToolExecutionCompletedData` derives it — so satisfying that would mean
/// adding derives to two protocol types to serve a frontend convenience. It
/// would also be a poor comparison: both carry `serde_json::Value` payloads,
/// where `==` is structural JSON equality and says nothing about whether two
/// entries are the same tool card. A tool entry's identity is its
/// `tool_call_id`, which is what the tests compare.
#[derive(Debug, Clone)]
pub(crate) enum MessageSegment {
    Content(String),
    Tools {
        entries: Vec<ToolRequestEntry>,
        /// True when this group sits at a recorded provider boundary, so it
        /// closes a phase. False for offsetless tools, which keep the legacy
        /// tail position and belong to no phase.
        placed: bool,
    },
    /// Where attached images belong. Kept as a slot rather than appended by the
    /// renderer so the legacy order — content, images, tools — survives, and so
    /// the position is decided in one testable place instead of falling out of
    /// the order of blocks in the view.
    Images,
}

/// Rebuild a message as the sequence of provider responses it actually was.
///
/// Splitting happens at exactly the recorded offset. It is tempting to snap to
/// a Markdown block boundary so each fragment is a whole document, but that is
/// the wrong model: the fragments are *already* whole documents, because each
/// one is a separate provider response. Snapping would move the tool card away
/// from the boundary the provider reported, which is the very thing this data
/// exists to record.
///
/// **Cross-phase Markdown deliberately does not resolve.** A link reference,
/// footnote definition, or code fence opened in one phase and closed in another
/// spans two different provider responses. Each phase is rendered as its own
/// document, so such a construct stays literal rather than being silently
/// joined into a document no provider produced. That is the honest rendering:
/// the model did not emit one document, and pretending otherwise would invent
/// structure — and, for assistive technology, invent relationships — that never
/// existed.
///
/// - Tools are matched to offsets by id.
/// - Offsets are Unicode scalar indices, resolved through `char_indices`, so a
///   split can never land inside a character.
/// - A tool with no offset — legacy data, or a backend that records none —
///   keeps the old layout: after all content **and after the images**, which is
///   exactly where it rendered before.
/// - An offset past the end is **rejected, not clamped**, and the tool is
///   treated as offsetless. Clamping would place the card at the end of the
///   content, which is a positional claim the data does not support: an
///   out-of-range offset says the sender's accounting is wrong, not that the
///   tool ran last. The backend guarantees the bound; malformed metadata falls
///   back to the legacy layout rather than inventing a position. A tool whose
///   id matches no call is treated the same way.
/// - Several tools at one offset keep their arrival order, and the sort is
///   stable, so equal offsets are not a reordering hazard.
pub(crate) fn interleave_message(
    content: &str,
    tool_calls: &[protocol::ToolUseData],
    tools: Vec<ToolRequestEntry>,
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

    // Valid offsets are `0..=scalar_len`: the end is a legitimate boundary,
    // meaning the tool was observed after all of this phase's text.
    let scalar_len = content.chars().count();
    let placement_for = |tool_call_id: &str| -> Option<usize> {
        tool_calls
            .iter()
            .find(|call| call.tool_call_id == tool_call_id)
            .and_then(|call| call.content_offset)
            .filter(|offset| (*offset as usize) <= scalar_len)
            .map(|offset| byte_at(offset as usize))
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
    let push_content = |segments: &mut Vec<MessageSegment>, slice: &str| {
        if !slice.is_empty() {
            segments.push(MessageSegment::Content(slice.to_owned()));
        }
    };

    if placed.is_empty() {
        // Legacy layout, unchanged: content, images, tools.
        push_content(&mut segments, content);
    } else {
        // Stable, so tools sharing an offset keep arrival order.
        placed.sort_by_key(|(byte, _)| *byte);
        let mut cursor = 0usize;
        let mut index = 0usize;
        while index < placed.len() {
            let byte = placed[index].0;
            if byte > cursor {
                push_content(&mut segments, &content[cursor..byte]);
                cursor = byte;
            }
            let mut entries = Vec::new();
            while index < placed.len() && placed[index].0 == byte {
                entries.push(placed[index].1.clone());
                index += 1;
            }
            segments.push(MessageSegment::Tools {
                entries,
                placed: true,
            });
        }
        push_content(&mut segments, &content[cursor..]);
    }

    if has_images {
        segments.push(MessageSegment::Images);
    }
    if !trailing.is_empty() {
        segments.push(MessageSegment::Tools {
            entries: trailing,
            placed: false,
        });
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
    entry: ArcRwSignal<ChatMessageEntry>,
) -> impl IntoView {
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
                let images = entry.message.images.clone();
                let segments = interleave_message(
                    &entry.message.content,
                    &entry.message.tool_calls,
                    entry.tool_requests,
                    images.as_ref().is_some_and(|imgs| !imgs.is_empty()),
                );

                // Each provider response is one group, so assistive technology
                // walks the card as the sequence of responses it was rather
                // than as one undifferentiated blob of text and cards. Groups
                // close on a placed tool, which is the boundary `tool.start`
                // reported. The trailing legacy tools and the images belong to
                // no response and stay outside.
                let mut rendered: Vec<AnyView> = Vec::new();
                let mut phase: Vec<AnyView> = Vec::new();
                let mut phase_index = 1usize;
                let mut body_ref_taken = false;
                let flush = |phase: &mut Vec<AnyView>, rendered: &mut Vec<AnyView>, index: usize| {
                    if phase.is_empty() {
                        return;
                    }
                    let parts = std::mem::take(phase);
                    rendered.push(
                        view! {
                            <div
                                class="chat-card-phase"
                                role="group"
                                aria-label=format!("Response {index}")
                            >
                                {parts}
                            </div>
                        }
                        .into_any(),
                    );
                };

                for segment in segments {
                    match segment {
                        MessageSegment::Content(text) => {
                            // Each phase is its own document: it was produced by
                            // its own provider call.
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
                                continue;
                            }
                            // The node ref identifies the row's body; with the
                            // body split it belongs to the first slice.
                            let view = if body_ref_taken {
                                view! {
                                    <div class="chat-card-body" inner_html=html></div>
                                }
                                .into_any()
                            } else {
                                body_ref_taken = true;
                                view! {
                                    <div
                                        class="chat-card-body"
                                        node_ref=body_ref
                                        inner_html=html
                                    ></div>
                                }
                                .into_any()
                            };
                            phase.push(view);
                        }
                        MessageSegment::Tools { entries, placed } => {
                            let view = view! {
                                <ToolCardListView agent_ref=agent_ref entries=entries />
                            }
                            .into_any();
                            if placed {
                                phase.push(view);
                                flush(&mut phase, &mut rendered, phase_index);
                                phase_index += 1;
                            } else {
                                flush(&mut phase, &mut rendered, phase_index);
                                rendered.push(view);
                            }
                        }
                        MessageSegment::Images => {
                            flush(&mut phase, &mut rendered, phase_index);
                            if let Some(imgs) = images.clone() {
                                rendered.push(render_message_images(imgs).into_any());
                            }
                        }
                    }
                }
                flush(&mut phase, &mut rendered, phase_index);
                rendered
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
    use crate::state::AppState;
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, ChatMessage, ChatMessageId, MessageMetadataUpdateData, MessageTokenUsage,
        TokenUsage, TokenUsageUnavailableReason,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-chat")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-chat");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn tool_call(id: &str, offset: Option<u32>) -> protocol::ToolUseData {
        protocol::ToolUseData {
            tool_call_id: id.to_owned(),
            name: "run".to_owned(),
            arguments: serde_json::json!({}),
            content_offset: offset,
        }
    }

    fn tool_entry(id: &str) -> ToolRequestEntry {
        ToolRequestEntry {
            tool_name: "run".to_owned(),
            request: protocol::ToolRequest {
                tool_call_id: id.to_owned(),
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
                MessageSegment::Tools { entries, placed } => format!(
                    "{}:{}",
                    if *placed { "tools" } else { "legacy" },
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
            vec![
                tool_entry("first"),
                tool_entry("second"),
                tool_entry("third"),
            ],
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
            false,
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:all of the text", "legacy:t1,t2"],
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
            false,
        );
        assert_eq!(
            segment_shape(&past_end),
            vec!["content:short", "legacy:t1"],
            "an out-of-range offset is rejected, not clamped: it is evidence the \
             sender's accounting is wrong, not that the tool ran last, so the \
             card falls back to the legacy tail rather than claiming a position"
        );

        let unmatched = interleave_message(
            "text",
            &[tool_call("other", Some(1))],
            vec![tool_entry("t1")],
            false,
        );
        assert_eq!(
            segment_shape(&unmatched),
            vec!["content:text", "legacy:t1"],
            "a tool with no matching call is placed as legacy data"
        );

        let zero = interleave_message(
            "text",
            &[tool_call("t1", Some(0))],
            vec![tool_entry("t1")],
            false,
        );
        assert_eq!(
            segment_shape(&zero),
            vec!["tools:t1", "content:text"],
            "offset zero puts the tool before all content, with no empty run"
        );

        let empty_content = interleave_message(
            "",
            &[tool_call("t1", Some(0))],
            vec![tool_entry("t1")],
            false,
        );
        assert_eq!(
            segment_shape(&empty_content),
            vec!["tools:t1"],
            "an empty message emits no empty content segment"
        );

        // The boundary between in-range and out-of-range: an offset *equal* to
        // the scalar length means "after all of this phase's text" and is
        // legitimate, so it is placed. One past it is not.
        let at_end = interleave_message(
            "abc",
            &[tool_call("t1", Some(3))],
            vec![tool_entry("t1")],
            false,
        );
        assert_eq!(
            segment_shape(&at_end),
            vec!["content:abc", "tools:t1"],
            "an offset at the end of the content is in range and placed"
        );
        let past_by_one = interleave_message(
            "abc",
            &[tool_call("t1", Some(4))],
            vec![tool_entry("t1")],
            false,
        );
        assert_eq!(
            segment_shape(&past_by_one),
            vec!["content:abc", "legacy:t1"],
            "one past the end is malformed and falls back to legacy"
        );
    }

    /// The offset is a provider-response boundary, so the split is exact — not
    /// snapped to a Markdown block. Snapping would move the card away from the
    /// boundary `tool.start` reported, which is the one thing this data exists
    /// to record.
    #[wasm_bindgen_test]
    fn the_split_lands_exactly_on_the_recorded_boundary() {
        // Offset 3 is mid-paragraph: no block boundary is anywhere near it.
        let segments = interleave_message(
            "one two three",
            &[tool_call("t1", Some(3))],
            vec![tool_entry("t1")],
            false,
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:one", "tools:t1", "content: two three"],
            "the boundary is honoured exactly, mid-paragraph or not"
        );
    }

    /// Each phase is a separate provider response, so each is well-formed on
    /// its own. A fence opened and closed within one phase renders normally
    /// even though the message as a whole is split.
    #[wasm_bindgen_test]
    fn each_phase_is_independently_well_formed() {
        let content = "```rust\nfn a() {}\n```\nAFTER";
        // Offset 21 is the start of `AFTER`, i.e. the provider boundary.
        let boundary = content.chars().count() - "AFTER".chars().count();
        let segments = interleave_message(
            content,
            &[tool_call("t1", Some(boundary as u32))],
            vec![tool_entry("t1")],
            false,
        );

        let MessageSegment::Content(first) = &segments[0] else {
            panic!("first segment is the opening phase: {segments:?}");
        };
        assert_eq!(
            first.matches("```").count(),
            2,
            "the first phase closes its own fence: {first:?}"
        );
        assert_eq!(
            segment_shape(&segments),
            vec![
                "content:```rust\nfn a() {}\n```\n",
                "tools:t1",
                "content:AFTER"
            ]
        );
    }

    /// A reference that crosses a provider boundary must stay unresolved. The
    /// definition and the use were produced by different provider calls, so
    /// joining them would invent a document — and a relationship for assistive
    /// technology — that no provider emitted.
    #[wasm_bindgen_test]
    fn a_cross_phase_reference_is_not_silently_joined() {
        // `[label]` is used in the first phase; its definition arrives in the
        // second, after the tool call.
        let content = "see [label]\n\n[label]: https://example.com";
        let boundary = "see [label]\n\n".chars().count();
        let segments = interleave_message(
            content,
            &[tool_call("t1", Some(boundary as u32))],
            vec![tool_entry("t1")],
            false,
        );

        assert_eq!(
            segment_shape(&segments),
            vec![
                "content:see [label]\n\n",
                "tools:t1",
                "content:[label]: https://example.com"
            ],
            "the two phases stay separate documents"
        );

        // Rendered independently, the reference does not resolve to a link.
        let MessageSegment::Content(first) = &segments[0] else {
            panic!("expected content first");
        };
        let html = render_markdown(first);
        assert!(
            !html.contains("<a "),
            "a definition from another provider response must not resolve the \
             reference in this one: {html}"
        );
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
            true,
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:text", "images", "legacy:t1"],
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
            true,
        );

        assert_eq!(
            segment_shape(&segments),
            vec![
                "content:A",
                "tools:placed",
                "content:B",
                "images",
                "legacy:legacy"
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
            false,
        );

        assert_eq!(
            segment_shape(&segments),
            vec!["content:A", "tools:placed", "content:B", "legacy:legacy"]
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

    fn user_msg(text: &str) -> ChatMessageEntry {
        ChatMessageEntry {
            message: ChatMessage {
                message_id: None,
                timestamp: 0,
                sender: MessageSender::User,
                content: text.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
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
            let entry = ArcRwSignal::new(entry.clone());
            view! { <ChatMessageView agent_ref=agent_ref entry=entry /> }
        })
        .forget();
        container
    }

    #[wasm_bindgen_test]
    async fn user_bubbles_fit_short_text_and_wrap_long_text() {
        ensure_styles_loaded();

        let short_container = mount_message(user_msg("Why?"));
        next_tick().await;
        let short_card: HtmlElement = short_container
            .query_selector(".chat-card-user")
            .unwrap()
            .expect("user card")
            .dyn_into()
            .unwrap();
        let short_body: HtmlElement = short_container
            .query_selector(".chat-card-body")
            .unwrap()
            .expect("user bubble")
            .dyn_into()
            .unwrap();
        let short_style = web_sys::window()
            .unwrap()
            .get_computed_style(&short_body)
            .unwrap()
            .expect("computed bubble style");
        let pixels = |property: &str| {
            short_style
                .get_property_value(property)
                .unwrap()
                .trim_end_matches("px")
                .parse::<f64>()
                .unwrap()
        };
        let one_line_height =
            pixels("line-height") + pixels("padding-top") + pixels("padding-bottom");
        let short_rect = short_body.get_bounding_client_rect();
        let short_card_rect = short_card.get_bounding_client_rect();
        assert!(
            short_rect.height() <= one_line_height + 1.0,
            "\"Why?\" must stay on one line at desktop width; bubble is {}px high \
             for a {}px single-line box (bubble width {}px, card width {}px)",
            short_rect.height(),
            one_line_height,
            short_rect.width(),
            short_card_rect.width(),
        );
        assert!(
            short_rect.width() < short_card_rect.width() / 2.0,
            "a short user bubble must fit its content, not fill the row"
        );

        let long_container = mount_message(user_msg(
            "This deliberately long user message must wrap onto multiple lines \
             while remaining inside the normal user bubble width limit at desktop size.",
        ));
        next_tick().await;
        let long_card: HtmlElement = long_container
            .query_selector(".chat-card-user")
            .unwrap()
            .expect("long user card")
            .dyn_into()
            .unwrap();
        let long_body: HtmlElement = long_container
            .query_selector(".chat-card-body")
            .unwrap()
            .expect("long user bubble")
            .dyn_into()
            .unwrap();
        let long_rect = long_body.get_bounding_client_rect();
        let long_card_rect = long_card.get_bounding_client_rect();
        assert!(
            long_rect.height() > one_line_height + 1.0,
            "long user text must wrap; bubble height was {}px",
            long_rect.height(),
        );
        assert!(
            long_rect.width() <= long_card_rect.width() * 0.85 + 1.0,
            "long user bubble must respect the 85% width limit; bubble {}px, card {}px",
            long_rect.width(),
            long_card_rect.width(),
        );
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

    /// Provider order in the accessibility tree: each response is one labelled
    /// group, and the groups follow the order the provider produced them.
    #[wasm_bindgen_test]
    async fn phases_are_grouped_and_labelled_in_provider_order() {
        let mut entry = assistant_msg(None);
        entry.message.content = "first\n\nsecond\n\nthird".to_owned();
        let first_break = "first\n\n".chars().count() as u32;
        let second_break = "first\n\nsecond\n\n".chars().count() as u32;
        entry.message.tool_calls = vec![
            tool_call("t1", Some(first_break)),
            tool_call("t2", Some(second_break)),
        ];
        entry.tool_requests = vec![tool_entry("t1"), tool_entry("t2")];
        let container = mount_message(entry);
        next_tick().await;

        let groups = container.query_selector_all(".chat-card-phase").unwrap();
        assert_eq!(groups.length(), 3, "three provider responses, three groups");
        let labels: Vec<String> = (0..groups.length())
            .filter_map(|index| groups.item(index))
            .filter_map(|node| node.dyn_into::<web_sys::Element>().ok())
            .map(|el| {
                assert_eq!(
                    el.get_attribute("role").as_deref(),
                    Some("group"),
                    "each phase is a group for assistive technology"
                );
                el.get_attribute("aria-label").unwrap_or_default()
            })
            .collect();
        assert_eq!(
            labels,
            vec!["Response 1", "Response 2", "Response 3"],
            "groups are labelled in provider order"
        );

        // Within the card, content and cards still read in provider order.
        let order = dom_order(&container);
        assert_eq!(
            order,
            vec!["body", "tools", "body", "tools", "body"],
            "reading order alternates content and card as the provider emitted"
        );
    }

    /// The live shape, through the real component: the card sits between the
    /// two runs and neither run loses text.
    #[wasm_bindgen_test]
    async fn live_pre_tool_post_renders_in_provider_order() {
        let mut entry = assistant_msg(None);
        entry.message.content = "PRE\nPOST".to_owned();
        entry.message.tool_calls = vec![tool_call("t1", Some(4))];
        entry.tool_requests = vec![tool_entry("t1")];
        let container = mount_message(entry);
        next_tick().await;

        assert_eq!(
            dom_order(&container),
            vec!["body", "tools", "body"],
            "PRE, card, POST — the order the provider produced"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("PRE") && text.contains("POST"),
            "no content is lost across the boundary, got: {text}"
        );
    }

    /// Reading order of the rendered card, as a screen reader would walk it.    /// Reading order of the rendered card, as a screen reader would walk it.
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
            let entry = row
                .message_entry()
                .expect("push_chat_entry builds a message row")
                .clone();
            *shared_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref: Signal<Option<crate::state::ActiveAgentRef>> =
                RwSignal::new(None).into();
            view! { <ChatMessageView agent_ref=agent_ref entry=entry /> }
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
