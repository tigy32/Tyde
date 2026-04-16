use leptos::prelude::*;

use crate::state::{AppState, ConnectionStatus, DockVisibility};

#[component]
pub fn Header() -> impl IntoView {
    let state = expect_context::<AppState>();

    let status_text_state = state.clone();
    let status_text = Memo::new(move |_| {
        let connected = status_text_state.active_connection_count();
        let total = status_text_state.total_host_count();
        if total == 0 {
            return "No hosts".to_string();
        }

        let selected = status_text_state.selected_host();
        let selected_status = status_text_state.selected_host_connection_status();
        let selected_label = selected
            .map(|host| host.label)
            .unwrap_or_else(|| "No host".to_string());

        match selected_status {
            ConnectionStatus::Connected => {
                format!("{connected}/{total} hosts connected · {selected_label}")
            }
            ConnectionStatus::Connecting => format!("Connecting to {selected_label}"),
            ConnectionStatus::Disconnected => {
                format!("{connected}/{total} hosts connected · {selected_label} offline")
            }
            ConnectionStatus::Error(message) => format!("{selected_label}: {message}"),
        }
    });

    let status_class_state = state.clone();
    let status_class =
        Memo::new(
            move |_| match status_class_state.selected_host_connection_status() {
                ConnectionStatus::Disconnected => "status-dot disconnected",
                ConnectionStatus::Connecting => "status-dot connecting",
                ConnectionStatus::Connected => "status-dot connected",
                ConnectionStatus::Error(_) => "status-dot error",
            },
        );

    let toggle_left = move |_| {
        state.left_dock.update(|dock| {
            *dock = match dock {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_right = move |_| {
        state.right_dock.update(|dock| {
            *dock = match dock {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_bottom = move |_| {
        state.bottom_dock.update(|dock| {
            *dock = match dock {
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
                <button class="header-btn" title="Toggle Left Dock" on:click=toggle_left>"Left"</button>
                <button class="header-btn" title="Toggle Bottom Dock" on:click=toggle_bottom>"Bottom"</button>
                <button class="header-btn" title="Toggle Right Dock" on:click=toggle_right>"Right"</button>
            </div>
        </header>
    }
}
