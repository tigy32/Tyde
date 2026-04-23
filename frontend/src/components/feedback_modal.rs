use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::state::AppState;

#[derive(Clone, PartialEq)]
enum FeedbackState {
    Idle,
    Submitting,
    Success,
    Error(String),
}

#[component]
pub fn FeedbackModal() -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = state.feedback_open;

    let text = RwSignal::new(String::new());
    let status = RwSignal::new(FeedbackState::Idle);

    let on_backdrop_click = move |_| {
        if status.get_untracked() != FeedbackState::Submitting {
            open.set(false);
            text.set(String::new());
            status.set(FeedbackState::Idle);
        }
    };

    let on_cancel = move |_| {
        if status.get_untracked() != FeedbackState::Submitting {
            open.set(false);
            text.set(String::new());
            status.set(FeedbackState::Idle);
        }
    };

    let on_submit = move |_| {
        let feedback = text.get_untracked();
        let feedback = feedback.trim().to_owned();
        if feedback.is_empty() {
            status.set(FeedbackState::Error(
                "Please enter some feedback.".to_owned(),
            ));
            return;
        }
        status.set(FeedbackState::Submitting);
        spawn_local(async move {
            match bridge::submit_feedback(feedback).await {
                Ok(()) => status.set(FeedbackState::Success),
                Err(e) => status.set(FeedbackState::Error(e)),
            }
        });
    };

    let on_input = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlTextAreaElement = target.unchecked_into();
        text.set(el.value());
    };

    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();

    Effect::new(move |_| {
        if open.get() {
            text.set(String::new());
            status.set(FeedbackState::Idle);
            if let Some(el) = textarea_ref.get() {
                let _ = el.focus();
            }
        }
    });

    let is_submitting = move || status.get() == FeedbackState::Submitting;
    let is_success = move || status.get() == FeedbackState::Success;

    view! {
        <Show when=move || open.get()>
            <div class="feedback-overlay" on:click=on_backdrop_click>
                <div class="feedback-modal" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                    <h3 class="feedback-title">"Send Feedback"</h3>
                    <p class="feedback-description">"Let us know what you think — bugs, ideas, anything."</p>

                    <Show when=move || !is_success()>
                        <textarea
                            node_ref=textarea_ref
                            class="feedback-textarea"
                            placeholder="Your feedback…"
                            rows=5
                            prop:value=move || text.get()
                            prop:disabled=is_submitting
                            on:input=on_input
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </Show>

                    {move || match status.get() {
                        FeedbackState::Idle => None,
                        FeedbackState::Submitting => None,
                        FeedbackState::Success => Some(view! {
                            <p class="feedback-status feedback-status-success">"Thanks for your feedback!"</p>
                        }.into_any()),
                        FeedbackState::Error(msg) => Some(view! {
                            <p class="feedback-status feedback-status-error">{msg}</p>
                        }.into_any()),
                    }}

                    <div class="feedback-actions">
                        <button
                            class="feedback-btn"
                            on:click=on_cancel
                        >
                            {move || if is_success() { "Close" } else { "Cancel" }}
                        </button>
                        <Show when=move || !is_success()>
                            <button
                                class="feedback-btn feedback-btn-primary"
                                prop:disabled=is_submitting
                                on:click=on_submit
                            >
                                {move || if is_submitting() { "Sending…" } else { "Send" }}
                            </button>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}
