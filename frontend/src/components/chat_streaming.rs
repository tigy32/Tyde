use leptos::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsCast;

use crate::components::tool_card::StreamingToolCardListView;
use crate::markdown::render_markdown;
use crate::state::StreamingState;

/// Stream-delta cadence at which we re-parse markdown for the in-flight
/// assistant response. The raw `streaming.text` signal updates on every
/// delta (often 50+ per second from a fast LLM). Re-running pulldown-cmark
/// and syntect on the full accumulated text per delta dominates main-thread
/// time during a response — long replies with code blocks will pin the
/// thread for hundreds of ms at a time. Throttling the rendered text to
/// roughly two frames smooths the stream visually while keeping the cost
/// bounded at ~16 markdown renders/sec.
const STREAMING_RENDER_INTERVAL_MS: i32 = 33;
const REASONING_RENDER_BYTE_CAP: usize = 40_000;

#[component]
pub fn ChatStreamingView(streaming: StreamingState) -> impl IntoView {
    let text = streaming.text;
    let reasoning = streaming.reasoning;
    let tool_requests = streaming.tool_requests;
    let agent_name = streaming.agent_name;
    let model = streaming.model;

    let throttled_text = throttled_string_signal(text.clone(), clone_text, None);
    let reasoning_open = RwSignal::new(false);
    let throttled_reasoning = throttled_string_signal(
        reasoning.clone(),
        capped_reasoning_text,
        Some(reasoning_open),
    );
    let throttled_reasoning_slot = StoredValue::new_local(throttled_reasoning);
    let reasoning_has_content = Memo::new(move |_| reasoning.with(|text| !text.is_empty()));

    let body_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    view! {
        <div class="chat-card chat-card-assistant chat-card-streaming">
            <div class="chat-card-header">
                <span class="chat-card-sender">{agent_name}</span>
                {model.map(|m| view! {
                    <span class="chat-card-model">{m}</span>
                })}
                <span class="streaming-dot"></span>
            </div>
            {move || {
                if !reasoning_has_content.get() {
                    None
                } else {
                    Some(view! {
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
                            </summary>
                            <Show when=move || reasoning_open.get()>
                                <pre class="reasoning-content">
                                    {move || throttled_reasoning_slot.with_value(|signal| signal.get())}
                                </pre>
                            </Show>
                        </details>
                    })
                }
            }}
            <div class="chat-card-body" node_ref=body_ref>
                {
                    let throttled = throttled_text.clone();
                    move || {
                        // Read through `with` so render_markdown sees the
                        // string in place rather than getting a fresh clone.
                        // Markdown re-rendering at 30Hz over a long
                        // response shouldn't also pay a per-tick String
                        // clone of the accumulated text.
                        throttled.with(|current| {
                            if current.is_empty() {
                                view! { <p class="chat-card-thinking">"Thinking\u{2026}"</p> }
                                    .into_any()
                            } else {
                                let html = render_markdown(current);
                                view! {
                                    <>
                                        <div inner_html=html></div>
                                        <span class="streaming-cursor"></span>
                                    </>
                                }
                                .into_any()
                            }
                        })
                    }
                }
            </div>
            {move || {
                if tool_requests.with(|tools| tools.is_empty()) {
                    None
                } else {
                    Some(view! {
                        <StreamingToolCardListView entries=tool_requests.clone() />
                    })
                }
            }}
        </div>
    }
}

fn throttled_string_signal(
    source: ArcRwSignal<String>,
    transform: fn(&str) -> String,
    enabled: Option<RwSignal<bool>>,
) -> ArcRwSignal<String> {
    let initial = if enabled.as_ref().is_none_or(|signal| signal.get_untracked()) {
        source.with_untracked(|text| transform(text))
    } else {
        String::new()
    };
    let throttled: ArcRwSignal<String> = ArcRwSignal::new(initial);
    let render_pending = Rc::new(Cell::new(false));
    let timer_cb = {
        let pending = render_pending.clone();
        let dest = throttled.clone();
        let src = source.clone();
        Rc::new(wasm_bindgen::closure::Closure::<dyn FnMut()>::new(
            move || {
                pending.set(false);
                dest.set(src.with_untracked(|text| transform(text)));
            },
        ))
    };

    let source_for_effect = source.clone();
    let timer_cb_for_effect = timer_cb.clone();
    Effect::new(move |_| {
        if let Some(enabled) = enabled.as_ref()
            && !enabled.get()
        {
            return;
        }
        source_for_effect.with(|_| ());
        if render_pending.get() {
            return;
        }
        render_pending.set(true);
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                timer_cb_for_effect.as_ref().as_ref().unchecked_ref(),
                STREAMING_RENDER_INTERVAL_MS,
            );
        }
    });

    throttled
}

fn clone_text(text: &str) -> String {
    text.to_owned()
}

fn capped_reasoning_text(text: &str) -> String {
    if text.len() <= REASONING_RENDER_BYTE_CAP {
        return text.to_owned();
    }

    let mut start = text.len() - REASONING_RENDER_BYTE_CAP;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    let mut capped = String::with_capacity(tail.len() + 4);
    capped.push_str("...\n");
    capped.push_str(tail);
    capped
}
