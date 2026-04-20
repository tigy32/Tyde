use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::command_palette::CommandPalette;
use crate::components::feedback_modal::FeedbackModal;
use crate::components::header::Header;
use crate::components::host_browser::HostBrowser;
use crate::components::project_rail::ProjectRail;
use crate::components::settings_panel::restore_appearance;
use crate::components::workbench::Workbench;
use crate::devtools;
use crate::dispatch::dispatch_envelope;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{
    Envelope, FrameKind, HelloPayload, PROTOCOL_VERSION, ProjectPath, ProjectRootPath, StreamPath,
    TYDE_VERSION,
};

fn generate_host_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    StreamPath(format!("/host/{id}"))
}

struct EventListenerHandle {
    window: web_sys::Window,
    event: &'static str,
    callback: wasm_bindgen::closure::Closure<dyn Fn(web_sys::Event)>,
}

impl EventListenerHandle {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            self.event,
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

thread_local! {
    static APP_LISTENERS_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static APP_LISTENER_TOKEN: Cell<u64> = const { Cell::new(0) };
    static HOST_LISTENER_HANDLES: RefCell<Vec<bridge::UnlistenHandle>> = const { RefCell::new(Vec::new()) };
    static DEVTOOLS_LISTENER_HANDLE: RefCell<Option<bridge::UnlistenHandle>> = const { RefCell::new(None) };
    static KEYDOWN_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
    static CLICK_LISTENER_HANDLE: RefCell<Option<EventListenerHandle>> = const { RefCell::new(None) };
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
    CLICK_LISTENER_HANDLE.with(|handle| {
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

    let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Ok(ev) = ev.dyn_into::<web_sys::KeyboardEvent>() else {
            return;
        };
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
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "keydown",
            callback,
        });
    });
}

/// Intercept link clicks inside rendered chat messages.
///
/// - File-like hrefs are opened in the Tyde file viewer.
/// - External URLs (http/https) are opened in the system browser.
/// - No link ever navigates the webview itself.
fn install_click_listener(state: AppState) {
    CLICK_LISTENER_HANDLE.with(|slot| {
        if let Some(existing) = slot.borrow_mut().take() {
            existing.remove();
        }
    });

    let callback = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Some(target) = ev.target() else { return };

        // Walk up from the click target to find an <a> element.
        let anchor: Option<web_sys::HtmlAnchorElement> = {
            let mut node: Option<web_sys::Element> = target.dyn_into::<web_sys::Element>().ok();
            loop {
                match node {
                    None => break None,
                    Some(el) => {
                        if let Ok(a) = el.clone().dyn_into::<web_sys::HtmlAnchorElement>() {
                            break Some(a);
                        }
                        node = el.parent_element();
                    }
                }
            }
        };

        let Some(anchor) = anchor else { return };

        // Only intercept links inside rendered chat content.
        let in_chat = anchor.closest(".chat-card-body").ok().flatten().is_some();
        if !in_chat {
            return;
        }

        let href = anchor.get_attribute("href").unwrap_or_default();
        if href.is_empty() {
            return;
        }

        ev.prevent_default();

        if href.starts_with("http://")
            || href.starts_with("https://")
            || href.starts_with("mailto:")
        {
            // External link → open in system browser / mail client.
            if let Some(window) = web_sys::window() {
                let _ = window.open_with_url_and_target(&href, "_blank");
            }
        } else {
            let roots = state
                .active_project_info_untracked()
                .map(|project| project.project.roots)
                .unwrap_or_default();
            if let Some(path) = resolve_chat_file_href(&href, &roots) {
                crate::actions::open_project_path(&state, path);
            } else {
                log::warn!("ignoring unsupported chat link target: {href}");
            }
        }
    });

    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("click", callback.as_ref().unchecked_ref());
    CLICK_LISTENER_HANDLE.with(|slot| {
        slot.borrow_mut().replace(EventListenerHandle {
            window,
            event: "click",
            callback,
        });
    });
}

fn resolve_chat_file_href(href: &str, project_roots: &[String]) -> Option<ProjectPath> {
    let decoded = percent_decode_path(href).unwrap_or_else(|| href.to_owned());
    let normalized = normalize_file_reference(&decoded)?;

    if let Some(path) = project_path_from_absolute(&normalized, project_roots) {
        return Some(path);
    }

    if is_absolute_path(&normalized) {
        return None;
    }

    let root = project_roots.first()?.clone();
    Some(ProjectPath {
        root: ProjectRootPath(root),
        relative_path: normalized,
    })
}

fn normalize_file_reference(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_scheme = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let without_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let without_line_suffix = strip_trailing_line_suffix(without_query);
    let normalized = without_line_suffix
        .trim_start_matches("./")
        .replace('\\', "/");

    if normalized.trim().is_empty() {
        return None;
    }

    Some(normalized)
}

fn strip_trailing_line_suffix(path: &str) -> &str {
    let mut candidate = path;
    for _ in 0..2 {
        let Some((prefix, suffix)) = candidate.rsplit_once(':') else {
            break;
        };
        if suffix.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = prefix;
        } else {
            break;
        }
    }
    candidate
}

fn project_path_from_absolute(path: &str, project_roots: &[String]) -> Option<ProjectPath> {
    for root in project_roots {
        let normalized_root = root.replace('\\', "/");
        if path == normalized_root {
            return None;
        }

        if let Some(rest) = path.strip_prefix(&normalized_root) {
            if !rest.starts_with('/') {
                continue;
            }
            let relative_path = rest.trim_start_matches('/');
            if relative_path.is_empty() {
                return None;
            }
            return Some(ProjectPath {
                root: ProjectRootPath(root.clone()),
                relative_path: relative_path.to_owned(),
            });
        }
    }

    None
}

fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        || matches!(
            path.as_bytes(),
            [drive, b':', b'/', ..] if drive.is_ascii_alphabetic()
        )
}

fn percent_decode_path(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let decoded = (decode_hex_nibble(high)? << 4) | decode_hex_nibble(low)?;
                bytes.push(decoded);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_chat_file_href;
    use protocol::{ProjectPath, ProjectRootPath};

    #[test]
    fn resolves_absolute_file_links_with_line_numbers() {
        let roots = vec!["/Users/mike/Tyde2".to_owned()];
        let resolved =
            resolve_chat_file_href("/Users/mike/Tyde2/server/src/agent/mod.rs:366", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "server/src/agent/mod.rs".to_owned(),
            })
        );
    }

    #[test]
    fn resolves_relative_file_links_with_line_and_column_numbers() {
        let roots = vec!["/Users/mike/Tyde2".to_owned()];
        let resolved = resolve_chat_file_href("./server/src/agent/mod.rs:366:8", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "server/src/agent/mod.rs".to_owned(),
            })
        );
    }

    #[test]
    fn resolves_percent_encoded_file_urls() {
        let roots = vec!["/Users/mike/Tyde2".to_owned()];
        let resolved =
            resolve_chat_file_href("file:///Users/mike/Tyde2/docs/My%20File.md#L12", &roots);

        assert_eq!(
            resolved,
            Some(ProjectPath {
                root: ProjectRootPath("/Users/mike/Tyde2".to_owned()),
                relative_path: "docs/My File.md".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_absolute_paths_outside_the_active_project() {
        let roots = vec!["/Users/mike/Tyde2".to_owned()];
        let resolved = resolve_chat_file_href("/tmp/outside.rs:12", &roots);

        assert_eq!(resolved, None);
    }
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
    let state_for_clicks = state.clone();
    Effect::new(move |_| {
        install_keydown_listener(state_for_keys.clone());
    });
    Effect::new(move |_| {
        install_click_listener(state_for_clicks.clone());
    });

    let state_for_feedback = state.clone();
    let open_feedback = move |_| {
        state_for_feedback.feedback_open.set(true);
    };

    view! {
        <div class="app-shell">
            <Header />
            <div class="app-body">
                <ProjectRail />
                <Workbench />
            </div>
            <button
                class="feedback-fab"
                title="Send feedback"
                on:click=open_feedback
            >
                "Send feedback"
            </button>
            <CommandPalette />
            <FeedbackModal />
            <HostBrowser />
        </div>
    }
}

async fn initialize_hosts(state: AppState, listener_token: u64) {
    let handles = install_host_listeners(state.clone())
        .await
        .expect("failed to install host listeners");

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
        .filter(|host| {
            host.auto_connect
                || matches!(host.transport, bridge::HostTransportConfig::LocalEmbedded)
        })
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
                Ok(envelope) => {
                    log::info!(
                        "host_frame_rx host={} stream={} seq={} kind={}",
                        event.host_id,
                        envelope.stream,
                        envelope.seq,
                        envelope.kind
                    );
                    dispatch_envelope(&line_state, &event.host_id, envelope)
                }
                Err(error) => panic!(
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
                if !matches!(
                    statuses.get(&event.host_id),
                    Some(ConnectionStatus::Error(_))
                ) {
                    statuses.insert(event.host_id.clone(), ConnectionStatus::Disconnected);
                }
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
            panic!("failed to load configured hosts: {error}");
        }
    }
}

pub async fn connect_one_host(state: AppState, host_id: String) {
    state.connection_statuses.update(|statuses| {
        statuses.insert(host_id.clone(), ConnectionStatus::Connecting);
    });

    if let Err(error) = bridge::connect_host(bridge::HostIdRequest {
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
