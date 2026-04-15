use leptos::prelude::*;

use crate::actions::begin_new_chat;
use crate::components::chat_view::ChatView;
use crate::components::diff_view::DiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::components::settings_panel::SettingsPanel;
use crate::state::{AppState, CenterView, ConnectionStatus};

use protocol::BackendKind;

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    let set_home = move |_| state.center_view.set(CenterView::Home);
    let set_chat = move |_| state.center_view.set(CenterView::Chat);
    let set_editor = move |_| state.center_view.set(CenterView::Editor);

    let tab_class = move |target: CenterView| {
        move || {
            if state.center_view.get() == target {
                "tab active"
            } else {
                "tab"
            }
        }
    };

    // New chat split button
    let menu_open = RwSignal::new(false);

    let is_connected =
        Memo::new(move |_| matches!(state.connection_status.get(), ConnectionStatus::Connected));

    let enabled_backends = Memo::new(move |_| {
        state
            .host_settings
            .get()
            .map(|s| s.enabled_backends)
            .unwrap_or_default()
    });

    let state_for_new_chat = state.clone();
    let on_new_chat = move |_| {
        begin_new_chat(&state_for_new_chat, None);
    };

    let on_toggle_menu = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        menu_open.update(|open| *open = !*open);
    };

    let close_menu = move |_: web_sys::MouseEvent| {
        menu_open.set(false);
    };

    view! {
        <div class="center-zone">
            <div class="tab-bar">
                <button class={tab_class(CenterView::Home)} on:click=set_home>"Home"</button>
                <button class={tab_class(CenterView::Chat)} on:click=set_chat>"Chat"</button>
                <button class={tab_class(CenterView::Editor)} on:click=set_editor>"Editor"</button>

                <div class="tab-bar-spacer"></div>

                <div class="new-chat-split">
                    <button
                        class="new-chat-btn"
                        title="New Chat"
                        disabled=move || !is_connected.get()
                        on:click=on_new_chat
                    >
                        "New Chat"
                    </button>
                    <button
                        class="new-chat-menu-trigger"
                        title="Choose backend for new chat"
                        disabled=move || !is_connected.get()
                        on:click=on_toggle_menu
                        aria-haspopup="menu"
                        aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    >
                        <svg width="10" height="6" viewBox="0 0 10 6" fill="none">
                            <path d="M1 1L5 5L9 1" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                        </svg>
                    </button>

                    <Show when=move || menu_open.get()>
                        <div class="new-chat-backdrop" on:click=close_menu></div>
                        <div class="new-chat-menu" role="menu">
                            {move || {
                                let backends = enabled_backends.get();
                                if backends.is_empty() {
                                    vec![view! {
                                        <div class="new-chat-menu-empty">
                                            "No backends enabled"
                                        </div>
                                    }.into_any()]
                                } else {
                                    backends.into_iter().map(|kind| {
                                        let label = backend_label(kind);
                                        let menu_state = expect_context::<AppState>();
                                        let on_click = move |_| {
                                            menu_open.set(false);
                                            begin_new_chat(&menu_state, Some(kind));
                                        };
                                        view! {
                                            <button
                                                class="new-chat-menu-item"
                                                role="menuitem"
                                                on:click=on_click
                                            >
                                                {format!("New {label} Chat")}
                                            </button>
                                        }.into_any()
                                    }).collect::<Vec<_>>()
                                }
                            }}
                        </div>
                    </Show>
                </div>
            </div>
            <div class="center-content">
                {move || match state.center_view.get() {
                    CenterView::Home => view! {
                        <div class="center-content-scroll">
                            <HomeView />
                        </div>
                    }.into_any(),
                    CenterView::Chat => view! {
                        <ChatView />
                    }.into_any(),
                    CenterView::Editor => {
                        let has_file = state.open_file.get().is_some();
                        if has_file {
                            view! { <FileView /> }.into_any()
                        } else {
                            view! { <DiffView /> }.into_any()
                        }
                    }
                }}
            </div>
            <SettingsPanel />
        </div>
    }
}
