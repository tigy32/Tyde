use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::command_palette::CommandPalette;
use crate::components::header::Header;
use crate::components::host_browser::HostBrowser;
use crate::components::project_rail::ProjectRail;
use crate::components::settings_panel::restore_appearance;
use crate::components::workbench::Workbench;
use crate::devtools;
use crate::dispatch::dispatch_envelope;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{Envelope, FrameKind, HelloPayload, PROTOCOL_VERSION, StreamPath, TYDE_VERSION};

fn generate_host_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    StreamPath(format!("/host/{id}"))
}

struct KeydownListenerHandle {
    window: web_sys::Window,
    callback: Closure<dyn Fn(web_sys::KeyboardEvent)>,
}

impl KeydownListenerHandle {
    fn remove(self) {
        let _ = self
            .window
            .remove_event_listener_with_callback("keydown", self.callback.as_ref().unchecked_ref());
    }
}

thread_local! {
    static APP_LISTENERS_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static APP_LISTENER_TOKEN: Cell<u64> = const { Cell::new(0) };
    static HOST_LISTENER_HANDLES: RefCell<Vec<bridge::UnlistenHandle>> = const { RefCell::new(Vec::new()) };
    static DEVTOOLS_LISTENER_HANDLE: RefCell<Option<bridge::UnlistenHandle>> = const { RefCell::new(None) };
    static KEYDOWN_LISTENER_HANDLE: RefCell<Option<KeydownListenerHandle>> = const { RefCell::new(None) };
}

fn set_app_listeners_active(active: bool) {
    APP_LISTENERS_ACTIVE.with(|value| value.set(active));
}

fn app_listeners_active() -> bool {
    APP_LISTENERS_ACTIVE.with(Cell::get)
}

fn begin_app_listener_lifecycle() -> u64 {
    clear_app_listeners();
    APP_LISTENER_TOKEN.with(|token| {
        let next = token.get().wrapping_add(1);
        token.set(next);
        next
    })
}

fn app_listener_token_is_current(token: u64) -> bool {
    app_listeners_active() && APP_LISTENER_TOKEN.with(|current| current.get() == token)
}

fn clear_app_listeners() {
    set_app_listeners_active(false);
    HOST_LISTENER_HANDLES.with(|handles| {
        for handle in handles.borrow_mut().drain(..) {
            handle.remove();
        }
    });
    DEVTOOLS_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
    KEYDOWN_LISTENER_HANDLE.with(|handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            handle.remove();
        }
    });
}

fn install_keydown_listener(state: AppState) {
    KEYDOWN_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let callback =
        Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
            let ctrl_or_meta = ev.ctrl_key() || ev.meta_key();
            match ev.key().as_str() {
                "k" if ctrl_or_meta => {
                    ev.prevent_default();
                    state.command_palette_open.update(|v| *v = !*v);
                }
                "," if ctrl_or_meta => {
                    ev.prevent_default();
                    state.settings_open.update(|v| *v = !*v);
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
    let _ = window.add_event_listener_with_callback("keydown", callback.as_ref().unchecked_ref());
    KEYDOWN_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut()
            .replace(KeydownListenerHandle { window, callback });
    });
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    restore_appearance(&state);
    provide_context(state.clone());

    let listener_token = begin_app_listener_lifecycle();
    set_app_listeners_active(true);
    on_cleanup(clear_app_listeners);

    let state_for_startup = state.clone();
    Effect::new(move |_| {
        let state = state_for_startup.clone();
        let listener_token = listener_token;
        spawn_local(async move {
            initialize_hosts(state, listener_token).await;
        });
    });

    Effect::new(move |_| {
        let listener_token = listener_token;
        spawn_local(async move {
            match devtools::install_listener().await {
                Ok(handle) => {
                    if app_listener_token_is_current(listener_token) {
                        DEVTOOLS_LISTENER_HANDLE.with(|slot| {
                            if let Some(existing) = slot.borrow_mut().replace(handle) {
                                existing.remove();
                            }
                        });
                    } else {
                        handle.remove();
                    }
                }
                Err(err) => {
                    log::error!("failed to install ui debug listener: {err}");
                }
            }
        });
    });

    let state_for_keys = state.clone();
    Effect::new(move |_| {
        install_keydown_listener(state_for_keys.clone());
    });

    view! {
        <div class="app-shell">
            <Header />
            <div class="app-body">
                <ProjectRail />
                <Workbench />
            </div>
            <CommandPalette />
            <HostBrowser />
        </div>
    }
}

async fn initialize_hosts(state: AppState, listener_token: u64) {
    let handles = match install_host_listeners(state.clone()).await {
        Ok(handles) => handles,
        Err(err) => {
            log::error!("failed to install host listeners: {err}");
            return;
        }
    };

    if app_listener_token_is_current(listener_token) {
        HOST_LISTENER_HANDLES.with(|slot| {
            slot.borrow_mut().extend(handles);
        });
    } else {
        for handle in handles {
            handle.remove();
        }
        return;
    }

    refresh_configured_hosts(&state).await;

    if !app_listener_token_is_current(listener_token) {
        return;
    }

    let auto_connect_hosts = state
        .configured_hosts
        .get_untracked()
        .into_iter()
        .filter(|host| host.auto_connect || host.id == "local")
        .map(|host| host.id)
        .collect::<Vec<_>>();

    for host_id in auto_connect_hosts {
        connect_one_host(state.clone(), host_id).await;
    }
}

async fn install_host_listeners(state: AppState) -> Result<Vec<bridge::UnlistenHandle>, String> {
    let mut handles = Vec::with_capacity(3);

    let line_state = state.clone();
    handles.push(
        bridge::listen_host_line(move |event| {
            match serde_json::from_str::<Envelope>(&event.line) {
                Ok(envelope) => dispatch_envelope(&line_state, &event.host_id, envelope),
                Err(error) => log::error!(
                    "failed to parse envelope from host {}: {error}",
                    event.host_id
                ),
            }
        })
        .await?,
    );

    let disconnect_state = state.clone();
    handles.push(
        bridge::listen_host_disconnected(move |event| {
            let reconnect_local = event.host_id == "local"
                && !matches!(
                    disconnect_state
                        .connection_statuses
                        .get_untracked()
                        .get(&event.host_id),
                    Some(ConnectionStatus::Connecting)
                );
            disconnect_state.connection_statuses.update(|statuses| {
                statuses.insert(event.host_id.clone(), ConnectionStatus::Disconnected);
            });
            disconnect_state.clear_host_runtime(&event.host_id);
            if reconnect_local {
                let state = disconnect_state.clone();
                spawn_local(async move {
                    log::warn!("local host disconnected; reconnecting");
                    connect_one_host(state, "local".to_string()).await;
                });
            }
        })
        .await?,
    );

    let error_state = state.clone();
    handles.push(
        bridge::listen_host_error(move |event| {
            log::error!("host {} error: {}", event.host_id, event.message);
            error_state.connection_statuses.update(|statuses| {
                statuses.insert(event.host_id, ConnectionStatus::Error(event.message));
            });
        })
        .await?,
    );

    Ok(handles)
}

pub async fn refresh_configured_hosts(state: &AppState) {
    match bridge::list_configured_hosts().await {
        Ok(store) => {
            state.configured_hosts.set(store.hosts.clone());
            state.selected_host_id.set(store.selected_host_id.clone());
            state.connection_statuses.update(|statuses| {
                statuses.retain(|host_id, _| store.hosts.iter().any(|host| &host.id == host_id));
                for host in &store.hosts {
                    statuses
                        .entry(host.id.clone())
                        .or_insert(ConnectionStatus::Disconnected);
                }
            });
        }
        Err(error) => {
            log::error!("failed to load configured hosts: {error}");
        }
    }
}

pub async fn connect_one_host(state: AppState, host_id: String) {
    state.connection_statuses.update(|statuses| {
        statuses.insert(host_id.clone(), ConnectionStatus::Connecting);
    });

    if let Err(error) = bridge::connect_host(bridge::ConnectHostRequest {
        host_id: host_id.clone(),
    })
    .await
    {
        log::error!("failed to connect host {}: {}", host_id, error);
        state.connection_statuses.update(|statuses| {
            statuses.insert(host_id, ConnectionStatus::Error(error));
        });
        return;
    }

    let host_stream = generate_host_stream();
    state.host_streams.update(|streams| {
        streams.insert(host_id.clone(), host_stream.clone());
    });

    let hello = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        client_name: "tyde-desktop".to_owned(),
        platform: "wasm".to_owned(),
    };

    if let Err(error) = send_frame(&host_id, host_stream, FrameKind::Hello, &hello).await {
        log::error!("failed to send hello to host {}: {}", host_id, error);
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                host_id,
                ConnectionStatus::Error(format!("failed to send hello: {error}")),
            );
        });
    }
}
