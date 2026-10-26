use leptos::prelude::*;

use crate::state::AppState;

/// Keep the functional chat tree mounted while its presentation changes.
#[component]
pub fn ChatPresentation(children: Children) -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        <div
            class="chat-view"
            data-chat-appearance=move || state.chat_appearance.get().id()
            data-chat-chrome=move || state.chat_appearance.get().uses_ink_chrome().then_some("ink")
        >
            <style>{include_str!("../../ink-chat.css")}</style>
            <style>{include_str!("../../notebook-chat.css")}</style>
            {children()}
        </div>
    }
}
