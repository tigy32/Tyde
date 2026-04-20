use leptos::prelude::*;

use crate::actions::begin_new_chat;
use crate::components::chat_view::ChatView;
use crate::components::diff_view::DiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::components::settings_panel::SettingsPanel;
use crate::state::{AppState, ConnectionStatus, TabContent, TabId};

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
fn TabButton(tab_id: TabId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let tab_data = move || {
        state
            .center_zone
            .with(|cz| cz.tabs.iter().find(|t| t.id == tab_id).cloned())
    };
    let is_active = move || {
        state
            .center_zone
            .with(|cz| cz.active_tab_id == Some(tab_id))
    };

    let on_click = move |_| state.activate_tab(tab_id);

    let is_closeable = move || tab_data().is_some_and(|t| t.closeable);

    view! {
        <button
            class=move || if is_active() { "tab active" } else { "tab" }
            on:click=on_click
        >
            <span class="tab-label">{move || tab_data().map(|t| t.label).unwrap_or_default()}</span>
            {move || is_closeable().then(|| {
                let on_close = move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    let state = expect_context::<AppState>();
                    state.close_tab(tab_id);
                };
                view! {
                    <span class="tab-close" on:click=on_close>
                        <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
                            <path d="M1 1L7 7M7 1L1 7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
                        </svg>
                    </span>
                }
            })}
        </button>
    }
}

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    // New chat split button
    let menu_open = RwSignal::new(false);

    let is_connected_state = state.clone();
    let is_connected = Memo::new(move |_| {
        matches!(
            is_connected_state.selected_host_connection_status(),
            ConnectionStatus::Connected
        )
    });

    let enabled_backends_state = state.clone();
    let enabled_backends = Memo::new(move |_| {
        enabled_backends_state
            .selected_host_settings()
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

    let tab_ids = move || {
        state
            .center_zone
            .with(|cz| cz.tabs.iter().map(|t| t.id).collect::<Vec<_>>())
    };

    let tab_bar_class = move || {
        if state.tabs_enabled.get() {
            "tab-bar"
        } else {
            "tab-bar tab-bar-hidden"
        }
    };

    view! {
        <div class="center-zone">
            <div class=tab_bar_class>
                {move || tab_ids().into_iter().map(|id| {
                    view! { <TabButton tab_id=id /> }
                }).collect_view()}

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
                {move || {
                    let cz = state.center_zone.get();
                    let active_tab = cz.active_tab_id
                        .and_then(|id| cz.tabs.iter().find(|t| t.id == id));
                    match active_tab.map(|t| &t.content) {
                        Some(TabContent::Home) => view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        }.into_any(),
                        Some(TabContent::Chat { agent_ref }) => {
                            state.active_agent.set(agent_ref.clone());
                            view! { <ChatView /> }.into_any()
                        }
                        Some(TabContent::File { path }) => {
                            view! { <FileView path=path.clone() /> }.into_any()
                        }
                        Some(TabContent::Diff { root, scope }) => {
                            view! { <DiffView root=root.clone() scope=*scope /> }.into_any()
                        }
                        None => view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        }.into_any(),
                    }
                }}
            </div>
            <SettingsPanel />
        </div>
    }
}
