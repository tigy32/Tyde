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
/// and syntect on the full accumulated text per delta dominates main-thread
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
    //
    // The timer callback is allocated *once* and reused across firings.
    // The previous version called `Closure::new(...).forget()` per
    // throttle tick — leaking ~150 closures per long response (~40KB
    // each in debug). The persistent Closure lives in an Rc captured
    // by both the scheduling Effect and stays alive via the
    // ChatStreamingView's reactive scope.
    let throttled_text: ArcRwSignal<String> = ArcRwSignal::new(text.get_untracked());
    let render_pending = Rc::new(Cell::new(false));
    let timer_cb = {
        let pending = render_pending.clone();
        let dest = throttled_text.clone();
        let src = text.clone();
        Rc::new(wasm_bindgen::closure::Closure::<dyn FnMut()>::new(
            move || {
                pending.set(false);
                dest.set(src.get_untracked());
            },
        ))
    };

    let text_for_effect = text.clone();
    let timer_cb_for_effect = timer_cb.clone();
    Effect::new(move |_| {
        // Subscribe without cloning the accumulated text on every
        // delta — `.with` tracks the dependency for free.
        text_for_effect.with(|_| ());
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
