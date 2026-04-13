use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{FrameKind, SendMessagePayload};


#[component]
pub fn ChatInput() -> impl IntoView {
    let state = expect_context::<AppState>();

    let can_send = move || {
        state.active_agent_id.get().is_some()
            && matches!(state.connection_status.get(), ConnectionStatus::Connected)
            && !state.chat_input.get().trim().is_empty()
    };

    let do_send = move || {
        let text = state.chat_input.get();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }

        let host_id = match state.host_id.get() {
            Some(id) => id,
            None => return,
        };

        let agent_id = match state.active_agent_id.get() {
            Some(id) => id,
            None => return,
        };

        // Find the agent's instance_stream
        let agents = state.agents.get();
        let instance_stream = match agents.iter().find(|a| a.agent_id == agent_id) {
            Some(a) => a.instance_stream.clone(),
            None => return,
        };

        // Clear input immediately
        state.chat_input.set(String::new());

        // Send the message — the server will echo it back as ChatEvent::MessageAdded
        spawn_local(async move {
            let payload = SendMessagePayload { message: text };
            if let Err(e) =
                send_frame(&host_id, instance_stream, FrameKind::SendMessage, &payload).await
            {
                log::error!("failed to send message: {e}");
            }
        });
    };

    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            do_send();
        }
    };

    let on_click_send = move |_| {
        do_send();
    };

    let on_input = move |ev: leptos::ev::Event| {
        let target = event_target_value(&ev);
        state.chat_input.set(target);
    };

    view! {
        <div class="chat-input-area">
            <textarea
                class="chat-textarea"
                placeholder="Type a message..."
                prop:value=move || state.chat_input.get()
                on:input=on_input
                on:keydown=on_keydown
                rows="1"
            />
            <button
                class="chat-send-btn"
                disabled=move || !can_send()
                on:click=on_click_send
            >
                <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
                    <path d="M2 8L14 8M14 8L9 3M14 8L9 13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                </svg>
            </button>
        </div>
    }
}
