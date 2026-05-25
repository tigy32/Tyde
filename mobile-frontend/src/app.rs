use std::cell::RefCell;
use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components;
use crate::dispatch::{dispatch_envelope, reset_inbound_seq_for_host};
use crate::send::{reset_seq_for_host, send_frame};
use crate::state::{
    AppMode, AppState, ConnectionStatus, LocalHostId, MobileShellError, MobileTab,
    PairedHostConnectionStatus, PairedHostSummary,
};
use protocol::MobileAccessErrorCode;

thread_local! {
    static SEEN_HOST_LINES: RefCell<HashSet<(LocalHostId, u64)>> =
        RefCell::new(HashSet::new());
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    crate::components::settings_view::restore_appearance(&state);
    provide_context(state.clone());

    install_event_listeners(state.clone());
    spawn_initial_paired_hosts_load(state.clone());
    install_app_mode_effect(state.clone());

    view! {
        <div class="mobile-app" data-theme=move || state.theme.get()>
            // Mounted in every app mode so a paste-failed-during-pairing or
            // listener-registration failure stays visible. (Phase C HIGH 4.)
            <components::MobileShellErrorBanner />
            {move || {
                let mode = state.app_mode.get();
                match mode {
                    AppMode::Onboarding => view! { <components::OnboardingView /> }.into_any(),
                    AppMode::Pairing(screen) => view! {
                        <components::PairingFlow screen=screen />
                    }
                    .into_any(),
                    AppMode::Workspace => view! {
                        <WorkspaceShell />
                    }
                    .into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn WorkspaceShell() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        {move || {
            // No active host yet — show the picker so the user can pick or
            // pair another. The picker also handles the empty-list case.
            let active = state.active_local_host_id.get();
            if active.is_none() {
                return view! { <components::PairedHostsPicker /> }.into_any();
            }
            view! { <ActiveHostShell /> }.into_any()
        }}
    }
}

#[component]
fn ActiveHostShell() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <components::ConnectionBanner />
        <div class="mobile-content">
            {move || {
                let viewing_chat = state.viewing_chat.get();
                if viewing_chat {
                    view! { <components::ChatView /> }.into_any()
                } else {
                    let tab = state.active_tab.get();
                    match tab {
                        MobileTab::Home => view! { <components::HomeView /> }.into_any(),
                        MobileTab::Agents => view! { <components::AgentsView /> }.into_any(),
                        MobileTab::Sessions => view! { <components::SessionsView /> }.into_any(),
                        MobileTab::Projects => view! { <components::ProjectsView /> }.into_any(),
                        MobileTab::Settings => view! { <components::SettingsView /> }.into_any(),
                    }
                }
            }}
        </div>
        <Show when=move || !state.viewing_chat.get()>
            <components::BottomNav />
        </Show>
    }
}

/// Keeps `app_mode` aligned with the `paired_hosts` and pairing-flow signals.
/// The pairing flow itself sets `AppMode::Pairing(...)` while it runs; this
/// effect only routes between `Onboarding` and `Workspace` based on whether
/// any paired hosts exist.
fn install_app_mode_effect(state: AppState) {
    Effect::new(move |_| {
        let hosts = state.paired_hosts.get();
        let mode = state.app_mode.get_untracked();
        match mode {
            AppMode::Pairing(_) => {
                // The pairing flow is responsible for transitioning out of
                // pairing; do not interfere.
            }
            AppMode::Onboarding => {
                if !hosts.is_empty() {
                    state.app_mode.set(AppMode::Workspace);
                }
            }
            AppMode::Workspace => {
                if hosts.is_empty() {
                    state.app_mode.set(AppMode::Onboarding);
                }
            }
        }
    });
}

fn spawn_initial_paired_hosts_load(state: AppState) {
    spawn_local(async move {
        match bridge::list_paired_hosts().await {
            Ok(hosts) => apply_paired_hosts_list(&state, hosts),
            Err(error) => {
                log::error!("list_paired_hosts failed: {error}");
                // Phase C HIGH 4: surface to the user, not just console.
                report_shell_error(
                    &state,
                    MobileAccessErrorCode::Internal,
                    format!("Failed to load paired hosts: {error}"),
                );
            }
        }
    });
}

fn report_shell_error(state: &AppState, code: MobileAccessErrorCode, message: String) {
    state
        .mobile_shell_error
        .set(Some(MobileShellError { code, message }));
}

fn install_event_listeners(state: AppState) {
    spawn_local(async move {
        let state_line = state.clone();
        register_listener(
            &state,
            "host-line",
            bridge::listen_host_line(move |event| handle_host_line_event(&state_line, event)).await,
        );

        let state_disconnected = state.clone();
        register_listener(
            &state,
            "host-disconnected",
            bridge::listen_host_disconnected(move |event| {
                log::info!("host disconnected: {}", event.host_id);
                let host = LocalHostId(event.host_id);
                apply_disconnect(&state_disconnected, &host, None);
            })
            .await,
        );

        let state_error = state.clone();
        register_listener(
            &state,
            "host-error",
            bridge::listen_host_error(move |event| {
                log::error!("host error on {}: {}", event.host_id, event.message);
                let host = LocalHostId(event.host_id);
                state_error.connection_statuses.update(|map| {
                    map.insert(host, ConnectionStatus::Error(event.message));
                });
            })
            .await,
        );

        let state_paired = state.clone();
        register_listener(
            &state,
            "paired-hosts-changed",
            bridge::listen_paired_hosts_changed(move |event| {
                apply_paired_hosts_list(&state_paired, event.hosts);
            })
            .await,
        );

        let state_status = state.clone();
        register_listener(
            &state,
            "paired-host-connection-status",
            bridge::listen_paired_host_connection_status(move |event| {
                apply_connection_status(&state_status, event.local_host_id, event.status);
            })
            .await,
        );

        let state_shell_error = state.clone();
        register_listener(
            &state,
            "mobile-shell-error",
            bridge::listen_mobile_shell_error(move |error| {
                log::error!("mobile shell error: {:?} {}", error.code, error.message);
                state_shell_error.mobile_shell_error.set(Some(error));
            })
            .await,
        );

        prepare_frontend_reattach(&state);
        if let Err(error) = bridge::frontend_attached().await {
            log::error!("frontend_attached failed: {error}");
            report_shell_error(
                &state,
                MobileAccessErrorCode::Internal,
                format!("Failed to refresh host connection after app attach: {error}"),
            );
        }

        match bridge::list_paired_host_connection_statuses().await {
            Ok(statuses) => {
                for event in statuses {
                    apply_connection_status(&state, event.local_host_id, event.status);
                }
                drain_pending_host_lines(state.clone());
            }
            Err(error) => {
                log::error!("list_paired_host_connection_statuses failed: {error}");
                report_shell_error(
                    &state,
                    MobileAccessErrorCode::Internal,
                    format!("Failed to load paired host connection statuses: {error}"),
                );
            }
        }
    });
}

fn handle_host_line_event(state: &AppState, event: bridge::HostLineEvent) {
    let host = LocalHostId(event.host_id);
    let delivery_id = event.delivery_id;
    if let Some(delivery_id) = delivery_id
        && !mark_host_line_seen(&host, delivery_id)
    {
        ack_host_line_delivery(host, delivery_id);
        return;
    }

    match serde_json::from_str::<protocol::Envelope>(&event.line) {
        Ok(envelope) => {
            log::info!(
                "mobile_frame_rx host={} stream={} seq={} kind={}",
                host,
                envelope.stream,
                envelope.seq,
                envelope.kind
            );
            dispatch_envelope(state, &host, envelope);
        }
        Err(error) => {
            let message = format!("Failed to parse host frame for {host}: {error}");
            log::error!("{message}");
            state.connection_statuses.update(|map| {
                map.insert(host.clone(), ConnectionStatus::Error(message.clone()));
            });
            report_shell_error(state, MobileAccessErrorCode::BrokerProtocol, message);
        }
    }

    if let Some(delivery_id) = delivery_id {
        ack_host_line_delivery(host, delivery_id);
    }
}

fn drain_pending_host_lines(state: AppState) {
    spawn_local(async move {
        match bridge::list_pending_host_lines().await {
            Ok(events) => {
                for event in events {
                    handle_host_line_event(&state, event);
                }
            }
            Err(error) => {
                log::error!("list_pending_host_lines failed: {error}");
                report_shell_error(
                    &state,
                    MobileAccessErrorCode::Internal,
                    format!("Failed to drain pending host frames: {error}"),
                );
            }
        }
    });
}

fn mark_host_line_seen(host: &LocalHostId, delivery_id: u64) -> bool {
    SEEN_HOST_LINES.with(|seen| seen.borrow_mut().insert((host.clone(), delivery_id)))
}

fn ack_host_line_delivery(host: LocalHostId, delivery_id: u64) {
    spawn_local(async move {
        if let Err(error) = bridge::ack_host_line(&host, delivery_id).await {
            log::error!("ack_host_line({host}, {delivery_id}) failed: {error}");
        }
    });
}

/// Either forgets the unlisten handle (success) or routes the registration
/// failure into `mobile_shell_error` so the user sees it. Phase C HIGH 4.
fn register_listener(
    state: &AppState,
    event_name: &str,
    result: Result<bridge::UnlistenHandle, String>,
) {
    match result {
        Ok(handle) => std::mem::forget(handle),
        Err(error) => {
            log::error!("failed to register {event_name} listener: {error}");
            report_shell_error(
                state,
                MobileAccessErrorCode::Internal,
                format!("Failed to register {event_name} listener: {error}"),
            );
        }
    }
}

fn prepare_frontend_reattach(state: &AppState) {
    let hosts = state
        .host_streams
        .get_untracked()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for host in hosts {
        apply_disconnect(state, &host, None);
    }
}

/// Replaces `state.paired_hosts` and reconciles dependent maps.
pub fn apply_paired_hosts_list(state: &AppState, hosts: Vec<PairedHostSummary>) {
    let known_ids: std::collections::HashSet<LocalHostId> =
        hosts.iter().map(|h| h.local_host_id.clone()).collect();
    state.paired_hosts.set(hosts);

    // Drop runtime entries for hosts that disappeared (e.g. forget).
    let tracked: Vec<LocalHostId> = state
        .connection_statuses
        .get_untracked()
        .keys()
        .cloned()
        .collect();
    for id in tracked {
        if !known_ids.contains(&id) {
            state.clear_host_runtime(&id);
            state.connection_statuses.update(|m| {
                m.remove(&id);
            });
            // Phase C HIGH 2: per-host seq + protocol validators must be
            // dropped when the host is forgotten so re-pairing doesn't
            // collide with stale state.
            reset_inbound_seq_for_host(&id);
            reset_seq_for_host(&id);
        }
    }

    if let Some(active) = state.active_local_host_id.get_untracked()
        && !known_ids.contains(&active)
    {
        state.active_local_host_id.set(None);
    }
}

fn apply_connection_status(
    state: &AppState,
    host: LocalHostId,
    status: PairedHostConnectionStatus,
) {
    let already_connected = matches!(
        state.connection_statuses.get_untracked().get(&host),
        Some(ConnectionStatus::Connected)
    ) && state.host_stream_untracked(&host).is_some();
    let connection: ConnectionStatus = status.clone().into();
    state.connection_statuses.update(|m| {
        m.insert(host.clone(), connection.clone());
    });
    match status {
        PairedHostConnectionStatus::Connected => {
            if already_connected {
                if state.active_local_host_id.get_untracked().is_none() {
                    state.active_local_host_id.set(Some(host));
                }
                return;
            }
            // Allocate a fresh host stream for this host and send Hello.
            let stream = make_host_stream();
            reset_inbound_seq_for_host(&host);
            reset_seq_for_host(&host);
            state.host_streams.update(|m| {
                m.insert(host.clone(), stream.clone());
            });
            let host_for_hello = host.clone();
            let state_for_hello_error = state.clone();
            spawn_local(async move {
                if let Err(error) = send_frame(
                    &host_for_hello,
                    stream,
                    protocol::FrameKind::Hello,
                    &protocol::HelloPayload {
                        protocol_version: protocol::PROTOCOL_VERSION,
                        tyde_version: protocol::TYDE_VERSION,
                        client_name: "tyde-mobile".to_string(),
                        platform: "ios".to_string(),
                    },
                )
                .await
                {
                    let message = format!("failed to send hello to {host_for_hello}: {error}");
                    log::error!("{message}");
                    report_shell_error(
                        &state_for_hello_error,
                        MobileAccessErrorCode::TransportFailed,
                        message,
                    );
                }
            });
            // If no host is currently selected, surface this one so the
            // workspace shows something useful as soon as a connection lands.
            if state.active_local_host_id.get_untracked().is_none() {
                state.active_local_host_id.set(Some(host));
            }
        }
        PairedHostConnectionStatus::Disconnected { reason } => {
            apply_disconnect(state, &host, Some(reason));
        }
        PairedHostConnectionStatus::Failed { message, .. } => {
            apply_disconnect(state, &host, Some(message));
        }
        PairedHostConnectionStatus::Connecting => {}
    }
}

fn apply_disconnect(state: &AppState, host: &LocalHostId, _reason: Option<String>) {
    state.connection_statuses.update(|m| {
        let entry = m
            .entry(host.clone())
            .or_insert(ConnectionStatus::Disconnected);
        if matches!(
            entry,
            ConnectionStatus::Connected | ConnectionStatus::Connecting
        ) {
            *entry = ConnectionStatus::Disconnected;
        }
    });
    reset_inbound_seq_for_host(host);
    reset_seq_for_host(host);
    // Clear protocol-level snapshots; the paired host record itself is
    // preserved (forget_paired_host is the only thing that removes it).
    state.host_streams.update(|m| {
        m.remove(host);
    });
    state.host_settings_by_host.update(|m| {
        m.remove(host);
    });
    state.command_errors_by_host.update(|m| {
        m.remove(host);
    });
    state.backend_setup_by_host.update(|m| {
        m.remove(host);
    });
    state.session_schemas_by_host.update(|m| {
        m.remove(host);
    });
    state.custom_agents_by_host.update(|m| {
        m.remove(host);
    });
    state.mcp_servers_by_host.update(|m| {
        m.remove(host);
    });
    state.steering_by_host.update(|m| {
        m.remove(host);
    });
    state.skills_by_host.update(|m| {
        m.remove(host);
    });
    state
        .projects
        .update(|projects| projects.retain(|p| p.local_host_id != *host));
    state
        .agents
        .update(|agents| agents.retain(|a| a.local_host_id != *host));
    state
        .sessions
        .update(|sessions| sessions.retain(|s| s.local_host_id != *host));
    state.file_tree.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.git_status.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.chat_messages.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.streaming_text.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.task_lists.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_message_queue.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_turn_active.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.transient_events.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_session_settings.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    if state
        .active_project
        .get_untracked()
        .as_ref()
        .is_some_and(|active| active.local_host_id == *host)
    {
        state.active_project.set(None);
    }
    if state
        .active_agent
        .get_untracked()
        .as_ref()
        .is_some_and(|active| active.local_host_id == *host)
    {
        state.active_agent.set(None);
        state.viewing_chat.set(false);
    }
}

fn make_host_stream() -> protocol::StreamPath {
    protocol::StreamPath(format!(
        "/host/{}",
        js_sys::Math::random().to_string().replace("0.", "m")
    ))
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::PairedHostSummary;
    use leptos::mount::mount_to;
    use mobile_shell_types::{
        BrokerAuthSummary as BrokerAuth, BrokerEndpointSummary as BrokerEndpoint,
        RoomIdSummary as RoomId,
    };
    use protocol::BrokerUrl;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn install_tauri_invoke_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function() {
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
        "#,
        )
        .expect("install tauri stub");
    }

    fn fixture_host(id: &str, label: &str) -> PairedHostSummary {
        PairedHostSummary {
            local_host_id: LocalHostId(id.to_owned()),
            host_label: label.to_owned(),
            broker: BrokerEndpoint {
                url: BrokerUrl::new("wss://broker.example.test/mqtt").unwrap(),
                auth: BrokerAuth::Anonymous,
            },
            room: RoomId("AQEBAQEBAQEBAQEBAQEBAQ".to_owned()),
            credential_fingerprint: "fp".to_owned(),
            auto_connect: false,
            last_connected_at_ms: None,
        }
    }

    #[wasm_bindgen_test]
    fn protocol_errors_surface_when_host_frame_cannot_parse() {
        let state = AppState::new();
        let host = LocalHostId("host-parse-error".to_owned());

        handle_host_line_event(
            &state,
            bridge::HostLineEvent {
                host_id: host.0.clone(),
                line: r#"{"stream":"/host/test","kind":"unknown_frame","seq":0,"payload":{}}"#
                    .to_owned(),
                delivery_id: None,
            },
        );

        assert!(matches!(
            state.connection_statuses.get_untracked().get(&host),
            Some(ConnectionStatus::Error(message)) if message.contains("Failed to parse host frame")
        ));
        let error = state
            .mobile_shell_error
            .get_untracked()
            .expect("shell error");
        assert_eq!(error.code, MobileAccessErrorCode::BrokerProtocol);
        assert!(error.message.contains("unknown_frame"));
    }

    #[wasm_bindgen_test]
    fn connected_status_replay_does_not_replace_host_stream() {
        install_tauri_invoke_stub();
        let state = AppState::new();
        let host = LocalHostId("host-connected-replay".to_owned());

        apply_connection_status(&state, host.clone(), PairedHostConnectionStatus::Connected);
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should have stream");

        apply_connection_status(&state, host.clone(), PairedHostConnectionStatus::Connected);
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should still have stream");

        assert_eq!(second_stream, first_stream);
    }

    #[wasm_bindgen_test]
    fn frontend_reattach_forces_connected_status_to_refresh_host_stream() {
        install_tauri_invoke_stub();
        let state = AppState::new();
        let host = LocalHostId("host-frontend-reattach".to_owned());

        apply_connection_status(&state, host.clone(), PairedHostConnectionStatus::Connected);
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should have stream");

        prepare_frontend_reattach(&state);
        assert!(
            state.host_stream_untracked(&host).is_none(),
            "reattach must drop the old host stream so the next Connected status sends Hello"
        );

        apply_connection_status(&state, host.clone(), PairedHostConnectionStatus::Connected);
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("reattached connected host should have a stream");

        assert_ne!(second_stream, first_stream);
    }

    #[wasm_bindgen_test]
    fn host_line_delivery_ids_are_deduplicated() {
        let host = LocalHostId("host-line-dedupe".to_owned());

        assert!(mark_host_line_seen(&host, 42));
        assert!(!mark_host_line_seen(&host, 42));
        assert!(mark_host_line_seen(&host, 43));
    }

    /// Empty `paired_hosts` → onboarding screen. Adding a host while
    /// `app_mode` is `Onboarding` flips the mode to `Workspace`, which renders
    /// the picker for the new host.
    #[wasm_bindgen_test]
    async fn routes_between_onboarding_and_picker_based_on_paired_hosts() {
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state.clone());
            install_app_mode_effect(state.clone());
            view! {
                {move || {
                    let mode = state.app_mode.get();
                    match mode {
                        AppMode::Onboarding => view! { <crate::components::OnboardingView /> }
                            .into_any(),
                        AppMode::Pairing(screen) => view! {
                            <crate::components::PairingFlow screen=screen />
                        }
                        .into_any(),
                        AppMode::Workspace => view! { <WorkspaceShell /> }.into_any(),
                    }
                }}
            }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Welcome to Tyde"),
            "expected onboarding view, got: {text}"
        );

        let state = state_handle.borrow().as_ref().unwrap().clone();
        state
            .paired_hosts
            .set(vec![fixture_host("h1", "Living Room")]);
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Welcome to Tyde"),
            "onboarding should be gone after pairing, got: {text}"
        );
        assert!(
            text.contains("Pick a Host") || text.contains("Living Room"),
            "expected picker view, got: {text}"
        );
    }
}
