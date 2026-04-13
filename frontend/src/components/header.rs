use leptos::prelude::*;

use crate::state::{AppState, ConnectionStatus, DockVisibility};

#[component]
pub fn Header() -> impl IntoView {
    let state = expect_context::<AppState>();

    let status_text = Memo::new(move |_| match state.connection_status.get() {
        ConnectionStatus::Disconnected => "Disconnected".to_owned(),
        ConnectionStatus::Connecting => "Connecting\u{2026}".to_owned(),
        ConnectionStatus::Connected => "Connected".to_owned(),
        ConnectionStatus::Error(msg) => format!("Error: {msg}"),
    });

    let status_class = Memo::new(move |_| match state.connection_status.get() {
        ConnectionStatus::Disconnected => "status-dot disconnected",
        ConnectionStatus::Connecting => "status-dot connecting",
        ConnectionStatus::Connected => "status-dot connected",
        ConnectionStatus::Error(_) => "status-dot error",
    });

    let toggle_left = move |_| {
        state.left_dock.update(|v| {
            *v = match v {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_right = move |_| {
        state.right_dock.update(|v| {
            *v = match v {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_bottom = move |_| {
        state.bottom_dock.update(|v| {
            *v = match v {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    view! {
        <header class="header">
            <div class="header-left">
                <span class="header-title">"Tyde"</span>
                <div class="header-status">
                    <span class={status_class}></span>
                    <span class="status-text">{status_text}</span>
                </div>
            </div>
            <div class="header-right">
                <button class="header-btn" title="Toggle Left Dock" on:click=toggle_left>"L"</button>
                <button class="header-btn" title="Toggle Right Dock" on:click=toggle_right>"R"</button>
                <button class="header-btn" title="Toggle Bottom Dock" on:click=toggle_bottom>"B"</button>
            </div>
        </header>
    }
}
