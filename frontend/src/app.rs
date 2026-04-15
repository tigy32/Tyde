use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::command_palette::CommandPalette;
use crate::components::header::Header;
use crate::components::project_rail::ProjectRail;
use crate::components::settings_panel::restore_appearance;
use crate::components::workbench::Workbench;
use crate::devtools;
use crate::dispatch::dispatch_envelope;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{
    Envelope, FrameKind, HelloPayload, ListSessionsPayload, PROTOCOL_VERSION, StreamPath,
    TYDE_VERSION,
};

const HOST_ID: &str = "local";

fn generate_host_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    StreamPath(format!("/host/{id}"))
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    restore_appearance(&state);
    provide_context(state.clone());

    // Auto-connect on mount
    let state_clone = state.clone();
    Effect::new(move |_| {
        let state = state_clone.clone();
        spawn_local(async move {
            connect_and_listen(state).await;
        });
    });

    Effect::new(move |_| {
        spawn_local(async move {
            match devtools::install_listener().await {
                Ok(handle) => {
                    std::mem::forget(handle);
                }
                Err(err) => {
                    log::error!("failed to install ui debug listener: {err}");
                }
            }
        });
    });

    // Request session list when connection becomes Connected
    let state_for_sessions = state.clone();
    Effect::new(move |_| {
        if state_for_sessions.connection_status.get() == ConnectionStatus::Connected {
            let state = state_for_sessions.clone();
            spawn_local(async move {
                let host_id = state.host_id.get_untracked();
                let host_stream = state.host_stream.get_untracked();
                if let (Some(hid), Some(hs)) = (host_id, host_stream)
                    && let Err(e) =
                        send_frame(&hid, hs, FrameKind::ListSessions, &ListSessionsPayload {}).await
                {
                    log::error!("failed to send ListSessions: {e}");
                }
            });
        }
    });

    // Global keyboard shortcuts
    let state_for_keys = state.clone();
    Effect::new(move |_| {
        let state = state_for_keys.clone();
        let cb =
            Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
                let ctrl_or_meta = ev.ctrl_key() || ev.meta_key();
                match ev.key().as_str() {
                    "k" if ctrl_or_meta => {
                        ev.prevent_default();
                        state.command_palette_open.update(|v: &mut bool| *v = !*v);
                    }
                    "," if ctrl_or_meta => {
                        ev.prevent_default();
                        state.settings_open.update(|v: &mut bool| *v = !*v);
                    }
                    "n" if ctrl_or_meta => {
                        ev.prevent_default();
                        crate::actions::begin_new_chat(&state, None);
                    }
                    "Escape" => {
                        if state.command_palette_open.get_untracked() {
                            state.command_palette_open.set(false);
                        } else if state.settings_open.get_untracked() {
                            state.settings_open.set(false);
                        }
                    }
                    _ => {}
                }
            });
        let window = web_sys::window().unwrap();
        let _ = window.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        // Intentional leak: keyboard listener lives for the entire app lifetime.
        cb.forget();
    });

    view! {
        <div class="app-shell">
            <Header />
            <div class="app-body">
                <ProjectRail />
                <Workbench />
            </div>
            <CommandPalette />
        </div>
    }
}

async fn connect_and_listen(state: AppState) {
    state.connection_status.set(ConnectionStatus::Connecting);
    state.host_id.set(Some(HOST_ID.to_owned()));

    // Connect to the local host (duplex stream, no socket)
    let connect_result = bridge::connect_host(bridge::ConnectHostRequest {
        host_id: HOST_ID.to_owned(),
    })
    .await;

    if let Err(e) = connect_result {
        log::error!("failed to connect: {e}");
        state.connection_status.set(ConnectionStatus::Error(e));
        return;
    }

    // Set up event listeners
    let state_for_line = state.clone();
    let _line_handle = bridge::listen_host_line(move |event| {
        if event.host_id != HOST_ID {
            return;
        }
        match serde_json::from_str::<Envelope>(&event.line) {
            Ok(envelope) => dispatch_envelope(&state_for_line, envelope),
            Err(e) => log::error!("failed to parse envelope: {e}"),
        }
    })
    .await;

    if let Err(e) = &_line_handle {
        log::error!("failed to listen for host-line events: {e}");
    }

    let state_for_disconnect = state.clone();
    let _disconnect_handle = bridge::listen_host_disconnected(move |event| {
        if event.host_id != HOST_ID {
            return;
        }
        state_for_disconnect
            .connection_status
            .set(ConnectionStatus::Disconnected);
        log::info!("host disconnected");
    })
    .await;

    let state_for_error = state.clone();
    let _error_handle = bridge::listen_host_error(move |event| {
        if event.host_id != HOST_ID {
            return;
        }
        log::error!("host error: {}", event.message);
        state_for_error
            .connection_status
            .set(ConnectionStatus::Error(event.message));
    })
    .await;

    // Send Hello frame
    let host_stream = generate_host_stream();
    state.host_stream.set(Some(host_stream.clone()));
    let hello = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        client_name: "tyde-desktop".to_owned(),
        platform: "wasm".to_owned(),
    };

    if let Err(e) = send_frame(HOST_ID, host_stream, FrameKind::Hello, &hello).await {
        log::error!("failed to send hello: {e}");
        state.connection_status.set(ConnectionStatus::Error(format!(
            "failed to send hello: {e}"
        )));
        return;
    }

    log::info!("hello sent, waiting for welcome");

    // Intentional leaks: these event listeners live for the entire app lifetime
    // (process lifetime = app lifetime in this desktop app). The UnlistenHandle's
    // JS unlisten callback is never invoked — listeners are never removed.
    if let Ok(h) = _line_handle {
        std::mem::forget(h);
    }
    if let Ok(h) = _disconnect_handle {
        std::mem::forget(h);
    }
    if let Ok(h) = _error_handle {
        std::mem::forget(h);
    }
}
