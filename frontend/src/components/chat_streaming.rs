use leptos::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsCast;

use crate::components::tool_card::ToolCardView;
use crate::markdown::render_markdown;
use crate::state::StreamingState;

/// Stream-delta cadence at which we re-parse markdown for the in-flight
/// assistant response. The raw `streaming.text` signal updates on every
/// delta (often 50+ per second from a fast LLM). Re-running pulldown-cmark
/// + syntect on the full accumulated text per delta dominates main-thread
/// time during a response — long replies with code blocks will pin the
/// thread for hundreds of ms at a time. Throttling the rendered text to
/// roughly two frames smooths the stream visually while keeping the cost
/// bounded at ~16 markdown renders/sec.
const STREAMING_RENDER_INTERVAL_MS: i32 = 33;

#[component]
pub fn ChatStreamingView(streaming: StreamingState) -> impl IntoView {
    let text = streaming.text;
    let reasoning = streaming.reasoning;
    let tool_requests = streaming.tool_requests;
    let agent_name = streaming.agent_name;
    let model = streaming.model;

    // Throttled mirror of `text`. Updated via `setTimeout` rather than
    // following each delta synchronously, so the rendered markdown
    // re-parses at most ~30Hz no matter how fast the model streams.
    let throttled_text: ArcRwSignal<String> = ArcRwSignal::new(text.get_untracked());
    let render_pending = Rc::new(Cell::new(false));
    let throttled_for_effect = throttled_text.clone();
    let text_for_effect = text.clone();
    Effect::new(move |_| {
        let _ = text_for_effect.get(); // subscribe to deltas
        if render_pending.get() {
            return;
        }
        render_pending.set(true);
        let pending = render_pending.clone();
        let dest = throttled_for_effect.clone();
        let src = text_for_effect.clone();
        let cb = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            pending.set(false);
            dest.set(src.get_untracked());
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                STREAMING_RENDER_INTERVAL_MS,
            );
        }
        // Closure must outlive the timer; `forget` leaks one closure per
        // throttle window. With ~30Hz cadence and an in-flight stream
        // typically ≤ a few seconds long, the resulting allocations are
        // a few hundred kb and freed when the page reloads. A long-term
        // fix would pool a single Closure per ChatStreamingView.
        cb.forget();
    });

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
                let current_reasoning = reasoning.get();
                if current_reasoning.is_empty() {
                    None
                } else {
                    Some(view! {
                        <details class="chat-card-reasoning" open>
                            <summary>
                                <span class="reasoning-icon">"💭"</span>
                                " Thinking"
                            </summary>
                            <pre class="reasoning-content">{current_reasoning}</pre>
                        </details>
                    })
                }
            }}
            <div class="chat-card-body" node_ref=body_ref>
                {
                    let throttled = throttled_text.clone();
                    move || {
                        let current = throttled.get();
                        if current.is_empty() {
                            view! { <p class="chat-card-thinking">"Thinking\u{2026}"</p> }.into_any()
                        } else {
                            let html = render_markdown(&current);
                            view! {
                                <>
                                    <div inner_html=html></div>
                                    <span class="streaming-cursor"></span>
                                </>
                            }.into_any()
                        }
                    }
                }
            </div>
            {move || {
                let tools = tool_requests.get();
                if tools.is_empty() {
                    None
                } else {
                    Some(view! {
                        <div class="chat-card-tools">
                            {tools.into_iter().map(|tr| {
                                view! { <ToolCardView entry=tr /> }
                            }).collect::<Vec<_>>()}
                        </div>
                    })
                }
            }}
        </div>
    }
}
