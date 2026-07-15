use std::cell::RefCell;
use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components;
use crate::dispatch::{dispatch_envelope, reset_inbound_seq_for_host};
use crate::send::{reset_seq_for_host, send_frame};
use crate::state::{
    AppMode, AppState, ConnectionStatus, LocalHostId, MobileServiceAuthState, MobileShellError,
    MobileTab, PairedHostConnectionStatus, PairedHostSummary, PairingOffer, PairingScreen,
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
    spawn_boot_pairing_handoff(state.clone());

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
        // Host-scoped, so a new-chat submission the transport lost stays
        // recoverable wherever the user navigates — it has no agent to live
        // under, and the client will not guess one. Above the content (and so
        // above the composer) it stays reachable with the keyboard open.
        <components::PendingSubmissions />
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

/// Completes PWA loader pairing/auth handoffs at boot. A stashed QR is consumed
/// and authoritatively classified here; when OAuth returns with a callback, its
/// one-time handoff is completed before the pairing screen mounts. A failed
/// callback only short-circuits to the `Failed` screen when there is NO pending
/// QR: with a still-valid pending managed QR the `AuthFailed` state rides into
/// the `ServiceAuth` screen instead, where `ServiceAuthCard` renders the
/// sign-in buttons so the user can retry with one tap rather than re-scanning.
/// If Safari lost the sessionStorage QR, the callback is still completed and
/// the signed-in app stays on its normal onboarding/host-picker screen for a
/// fresh scan. Invalid stashed QRs are rejected rather than trusted as loader
/// decisions.
fn spawn_boot_pairing_handoff(state: AppState) {
    let pending_qr_uri = bridge::take_pending_pairing_uri();
    spawn_local(async move {
        let callback_auth = bridge::complete_boot_managed_auth_callback().await;
        match pending_qr_uri {
            Some(qr_uri) => match bridge::classify_pairing_offer(&qr_uri).await {
                Ok(offer) => {
                    let screen = boot_pairing_screen(qr_uri, offer, callback_auth);
                    state.app_mode.set(AppMode::Pairing(screen));
                }
                Err(error) => {
                    // Forged or stale handoff: drop the QR, but still surface the
                    // auth callback outcome exactly as if no QR were pending.
                    log::warn!("ignoring loader pairing handoff: {error}");
                    if let Some(auth) = callback_auth {
                        apply_boot_auth_without_pending(&state, auth);
                    }
                }
            },
            None => {
                if let Some(auth) = callback_auth {
                    apply_boot_auth_without_pending(&state, auth);
                }
            }
        }
    });
}

/// Routes a boot-time pending QR onto its pairing screen. The managed arm keeps
/// whatever terminal auth state the OAuth callback produced — including
/// `AuthFailed` — so the QR is not discarded and `ServiceAuthCard` can offer
/// sign-in/retry for it directly. Direct-pairing and repair offers don't involve
/// Tyggs auth, so the callback state is irrelevant to their screens.
fn boot_pairing_screen(
    qr_uri: String,
    offer: PairingOffer,
    callback_auth: Option<MobileServiceAuthState>,
) -> PairingScreen {
    match offer {
        PairingOffer::ManagedService { host_label } => PairingScreen::ServiceAuth {
            qr_uri,
            host_label,
            auth: callback_auth.unwrap_or(MobileServiceAuthState::Idle),
        },
        PairingOffer::RepairRequired { message } => PairingScreen::RepairRequired { message },
        PairingOffer::DirectPairing { preview } => PairingScreen::Confirm { qr_uri, preview },
    }
}

fn apply_boot_auth_without_pending(state: &AppState, auth: MobileServiceAuthState) {
    match auth {
        MobileServiceAuthState::Authenticated { .. } => {
            log::info!("completed Tyggs sign-in; scan a pairing QR to continue");
        }
        MobileServiceAuthState::AuthFailed { message } => {
            state
                .app_mode
                .set(AppMode::Pairing(PairingScreen::Failed { message }));
        }
        auth @ (MobileServiceAuthState::PassRequired { .. }
        | MobileServiceAuthState::ServiceUnavailable { .. }) => {
            state
                .app_mode
                .set(AppMode::Pairing(PairingScreen::ServiceAuthStatus { auth }));
        }
        MobileServiceAuthState::Idle | MobileServiceAuthState::Authenticating => {
            log::warn!("Tyggs OAuth callback did not produce a terminal auth state");
        }
    }
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
                apply_host_error(&state_error, LocalHostId(event.host_id), event.message);
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
                apply_connection_status(
                    &state_status,
                    event.local_host_id,
                    event.status,
                    event.connection_instance_id,
                );
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

        let state_submission = state.clone();
        register_listener(
            &state,
            "submission-transport-outcome",
            bridge::listen_submission_transport_outcome(move |event| {
                log::info!(
                    "submission_transport_outcome host={} connection_instance_id={} \
                     local_submission_id={} outcome={:?}",
                    event.local_host_id,
                    event.connection_instance_id,
                    event.local_submission_id.0,
                    event.outcome
                );
                state_submission.apply_submission_outcome(
                    event.local_submission_id,
                    event.connection_instance_id,
                    event.outcome,
                );
            })
            .await,
        );

        let known_connection_instance_ids = state
            .active_connection_instance_ids
            .get_untracked()
            .into_iter()
            .map(
                |(local_host_id, connection_instance_id)| bridge::KnownConnectionInstance {
                    local_host_id,
                    connection_instance_id,
                },
            )
            .collect::<Vec<_>>();
        if let Err(error) = bridge::frontend_attached(&known_connection_instance_ids).await {
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
                    apply_connection_status(
                        &state,
                        event.local_host_id,
                        event.status,
                        event.connection_instance_id,
                    );
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
    if let Some(connection_instance_id) = event.connection_instance_id {
        match state
            .active_connection_instance_ids
            .get_untracked()
            .get(&host)
            .copied()
        {
            Some(active_connection_instance_id)
                if active_connection_instance_id != connection_instance_id =>
            {
                log::info!(
                    "dropping stale host line host={} connection_instance_id={} active_connection_instance_id={}",
                    host,
                    connection_instance_id,
                    active_connection_instance_id
                );
                if let Some(delivery_id) = event.delivery_id {
                    mark_host_line_seen(&host, delivery_id);
                    ack_host_line_delivery(host, delivery_id);
                }
                return;
            }
            Some(_) => {}
            None if event.delivery_id.is_some() => {
                log::info!(
                    "deferring host line until connection status arrives host={} connection_instance_id={}",
                    host,
                    connection_instance_id
                );
                return;
            }
            None => {}
        }
    }
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
            // Report the failure back to the host so the server logs it with
            // the raw offending frame — invisible otherwise (it only reached
            // the WKWebView console + a clipped banner). The outgoing
            // ClientError frame travels on the same host stream and shares
            // the same per-stream seq counter as Hello/SendMessage, so it
            // cannot itself be re-parsed inbound and trigger another report.
            emit_client_parse_error(state, &host, message.clone(), event.line.clone());
            report_shell_error(state, MobileAccessErrorCode::BrokerProtocol, message);
        }
    }

    if let Some(delivery_id) = delivery_id {
        ack_host_line_delivery(host, delivery_id);
    }
}

/// Emits a `ClientError` frame back to the host on its stream so the server
/// can log the parse failure together with the raw offending frame line.
/// Uses the existing per-(host, stream) outgoing seq counter via `send_frame`
/// — no parallel send path or seq counter. If no host stream is allocated yet
/// (failure arrived before the connection finished bootstrapping) or the send
/// itself fails, the error is logged locally and not retried.
fn emit_client_parse_error(
    state: &AppState,
    host: &LocalHostId,
    message: String,
    raw_line: String,
) {
    let Some(stream) = state.host_stream_untracked(host) else {
        log::error!("cannot report client parse error to {host}: no host stream allocated yet");
        return;
    };
    let host = host.clone();
    spawn_local(async move {
        let payload = protocol::ClientErrorPayload {
            code: protocol::ClientErrorCode::ProtocolParse,
            message,
            raw_context: Some(raw_line),
        };
        if let Err(error) =
            send_frame(&host, stream, protocol::FrameKind::ClientError, &payload).await
        {
            log::error!("failed to report client parse error to {host}: {error}");
        }
    });
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

#[cfg(all(test, target_arch = "wasm32"))]
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
    connection_instance_id: Option<u64>,
) {
    // A sticky app-level `UpdateRequired` outranks any transport status. The
    // host genuinely speaks a protocol this build cannot, and that stays true
    // across transport reconnect churn (Connecting/Connected/Disconnected/
    // Failed), so none of those may overwrite it — nor re-run the Connected
    // branch below, which would allocate a fresh stream and re-send `Hello`
    // only to be rejected again. Only a successful `Welcome` (a compatible
    // reconnect, handled in `dispatch`) or forgetting the host clears it.
    if matches!(
        state.connection_statuses.get_untracked().get(&host),
        Some(ConnectionStatus::UpdateRequired { .. })
    ) {
        return;
    }

    match status {
        PairedHostConnectionStatus::Connected => {
            // Determine whether this Connected event refers to the same
            // underlying MQTT connection we already set up.  If so there is
            // no need to allocate a new host stream or send Hello — doing so
            // would reset protocol seq state and trigger a full rebootstrap
            // for no reason (e.g. the frontend listener was reattached while
            // the web connection stayed alive).
            let same_connection = match connection_instance_id {
                Some(id) => {
                    // The connection manager provides an instance id: use it
                    // as the authoritative check.
                    state
                        .active_connection_instance_ids
                        .get_untracked()
                        .get(&host)
                        .copied()
                        == Some(id)
                        && state.host_stream_untracked(&host).is_some()
                }
                None => {
                    // Older native binary with no instance id: fall back to
                    // the heuristic used before instance tracking existed.
                    matches!(
                        state.connection_statuses.get_untracked().get(&host),
                        Some(ConnectionStatus::Connected | ConnectionStatus::Bootstrapping)
                    ) && state.host_stream_untracked(&host).is_some()
                }
            };

            if same_connection {
                state.connection_statuses.update(|m| {
                    let status = if state.host_bootstrap_applied_for_current_stream_untracked(&host)
                    {
                        ConnectionStatus::Connected
                    } else {
                        ConnectionStatus::Bootstrapping
                    };
                    m.insert(host.clone(), status);
                });
                // Same live connection: just ensure a host is selected.
                if state.active_local_host_id.get_untracked().is_none() {
                    state.active_local_host_id.set(Some(host.clone()));
                }
                drain_pending_host_lines(state.clone());
                return;
            }

            state.connection_statuses.update(|m| {
                m.insert(host.clone(), ConnectionStatus::Bootstrapping);
            });
            state.bootstrapped_host_streams.update(|m| {
                m.remove(&host);
            });
            // New or changed connection: allocate a fresh host stream and
            // send Hello.  Intentionally do NOT pre-clear host data here —
            // apply_host_bootstrap will replace all bootstrap-owned slices
            // atomically when it arrives, avoiding a visible flash between
            // clear and refill.
            let stream = make_host_stream();
            reset_inbound_seq_for_host(&host);
            reset_seq_for_host(&host);
            if let Some(id) = connection_instance_id {
                state.active_connection_instance_ids.update(|m| {
                    m.insert(host.clone(), id);
                });
            }
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
            if state.active_local_host_id.get_untracked().is_none() {
                state.active_local_host_id.set(Some(host));
            }
            drain_pending_host_lines(state.clone());
        }
        PairedHostConnectionStatus::Disconnected { reason } => {
            state.connection_statuses.update(|m| {
                m.insert(host.clone(), ConnectionStatus::Disconnected);
            });
            // Terminal: clear tracked instance so the next Connected event
            // unconditionally starts a fresh protocol session.
            state.active_connection_instance_ids.update(|m| {
                m.remove(&host);
            });
            apply_disconnect(state, &host, Some(reason));
        }
        PairedHostConnectionStatus::Failed { code, message } => {
            let status = crate::state::connection_failed_status(code, message.clone());
            state.connection_statuses.update(|m| {
                m.insert(host.clone(), status);
            });
            state.active_connection_instance_ids.update(|m| {
                m.remove(&host);
            });
            apply_disconnect(state, &host, Some(message));
        }
        PairedHostConnectionStatus::Connecting => {
            state.connection_statuses.update(|m| {
                m.insert(host.clone(), ConnectionStatus::Connecting);
            });
            // Transient reconnect: status signal already updated above.
            // Keep all reactive state visible so the UI shows stale-but-
            // present data rather than a blank screen while the connection
            // re-establishes.
        }
    }
}

/// Records a transport-level host error unless a sticky app-level
/// `UpdateRequired` is already in effect. Overwriting `UpdateRequired` with a
/// transient `Error` would let the next transport `Connected` re-send `Hello`
/// and reopen the incompatible-protocol reject loop the sticky state exists to
/// stop, so the terminal handshake verdict outranks a transport error.
fn apply_host_error(state: &AppState, host: LocalHostId, message: String) {
    state.connection_statuses.update(|map| {
        if matches!(
            map.get(&host),
            Some(ConnectionStatus::UpdateRequired { .. })
        ) {
            return;
        }
        map.insert(host, ConnectionStatus::Error(message));
    });
}

fn apply_disconnect(state: &AppState, host: &LocalHostId, _reason: Option<String>) {
    state.active_connection_instance_ids.update(|m| {
        m.remove(host);
    });
    state.connection_statuses.update(|m| {
        let entry = m
            .entry(host.clone())
            .or_insert(ConnectionStatus::Disconnected);
        if matches!(
            entry,
            ConnectionStatus::Connected
                | ConnectionStatus::Connecting
                | ConnectionStatus::Bootstrapping
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
    state.bootstrapped_host_streams.update(|m| {
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
    // Both halves of the load latch must go. `agent_load_requests` alone is not
    // enough: `agent_loaded` is only ever written by an `AgentBootstrap`, and
    // the next connection mints a *new* agent instance stream, so a surviving
    // `agent_loaded` entry marks a chat as bootstrapped against a stream that no
    // longer exists. `ChatView` would then render an empty transcript as
    // "loaded and genuinely empty" instead of re-requesting the bootstrap.
    state.agent_load_requests.update(|m| {
        m.retain(|k| k.local_host_id != *host);
    });
    state.agent_loaded.update(|m| {
        m.retain(|k| k.local_host_id != *host);
    });
    // Clearing the load error alongside the latch is what makes a reconnect the
    // recovery action: the next connection's auto-load re-sends `LoadAgent` on
    // the fresh instance stream instead of re-rendering a stale failure.
    state.agent_load_errors.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state
        .sessions
        .update(|sessions| sessions.retain(|s| s.local_host_id != *host));
    state.session_lists_by_host.update(|m| {
        m.remove(host);
    });
    state.file_tree.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.git_status.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.chat_messages.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.chat_message_index.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.session_history.update(|m| {
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
    // `active_agent` and `viewing_chat` deliberately survive a disconnect.
    //
    // They are *navigation* state — where the user is standing — not server state,
    // and every byte of server state above has just been dropped, so nothing stale
    // is being kept authoritative. Clearing them silently teleported the user out
    // of the chat they were reading, which had two costs: the terminal
    // "connection dropped → Reconnect" state inside the chat became unreachable
    // (`ChatView` had already unmounted by the time anyone could render it), and a
    // transient blip threw them back to the tab list for no reason.
    //
    // Staying put means `ChatView` renders the disconnected terminal state, and a
    // reconnect re-bootstraps the host, re-loads this agent on its fresh instance
    // stream, and puts the user back in their conversation.
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

    fn empty_host_settings() -> protocol::HostSettings {
        protocol::HostSettings {
            enabled_backends: Vec::new(),
            default_backend: None,
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: Default::default(),
            background_agent_features: Default::default(),
            code_intel: Default::default(),
            backend_config: Default::default(),
            launch_profiles: Vec::new(),
        }
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

    /// **The real disconnect lifecycle must leave the user somewhere they can act.**
    ///
    /// `apply_disconnect` used to clear `active_agent` and `viewing_chat`, which
    /// unmounted `ChatView` outright. Every terminal recovery state inside the
    /// chat was therefore unreachable in the one lifecycle that actually happens:
    /// the chat-level test could set a `Disconnected` status directly and see the
    /// recovery, but a genuine `HostDisconnected` event teleported the user away
    /// before anything could render it.
    ///
    /// So this drives the *real* handler into a *real* mounted `ChatView`.
    ///
    /// Server state must still be dropped — nothing stale may be presented as
    /// authoritative — while the navigation pointer survives, so the user stays
    /// in the conversation and is offered the reconnect that fixes it.
    #[wasm_bindgen_test]
    async fn a_real_disconnect_leaves_an_actionable_recovery_in_the_open_chat() {
        let container = make_container();
        let host = LocalHostId("host-disconnect".to_owned());
        let host_for_mount = host.clone();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let mount = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = crate::state::AgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
            };
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|m| {
                m.insert(host_for_mount.clone(), ConnectionStatus::Connected);
            });
            state.host_streams.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    protocol::StreamPath("/host/h1".to_owned()),
                );
            });
            state.agents.set(vec![crate::state::AgentInfo {
                local_host_id: host_for_mount.clone(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
                name: "Coder".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: protocol::StreamPath("/agent/a1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: protocol::AgentId("agent-1".to_owned()),
            }));
            state.viewing_chat.set(true);
            // The chat is mid-load: the latch is set and no snapshot has landed.
            // This is exactly the state the user reported as an endless spinner.
            state.agent_load_requests.update(|m| {
                m.insert(agent_ref);
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <crate::components::ChatView /> }
        });
        std::mem::forget(mount);
        next_tick().await;
        let state = handle.borrow().as_ref().unwrap().clone();

        // A message the user discarded *before* the drop. Its tombstone is the only
        // thing stopping the tool card that sent it from going back to claiming it
        // is on its way to the agent — so a disconnect must not take it.
        let discarded = crate::state::SubmissionOriginId(999);
        state.hold_submission(crate::state::PendingSubmission {
            local_submission_id: crate::bridge::LocalSubmissionId(99),
            origin: discarded,
            local_host_id: host.clone(),
            connection_instance_id: 7,
            target: crate::state::SubmissionTarget::NewChat,
            text: "thrown away".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: crate::state::PendingSubmissionState::NotSent,
        });
        state.withdraw_submission(
            crate::bridge::LocalSubmissionId(99),
            crate::state::SubmissionWithdrawal::Discarded,
        );

        // A message that was in flight when the link died — it must survive, because
        // its record is the only holder of the user's text.
        state.hold_submission(crate::state::PendingSubmission {
            local_submission_id: crate::bridge::LocalSubmissionId(1),
            origin: state.mint_submission_origin(),
            local_host_id: host.clone(),
            connection_instance_id: 7,
            target: crate::state::SubmissionTarget::NewChat,
            text: "do not lose me".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: crate::state::PendingSubmissionState::QueuedLocally,
        });

        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading-spinner']")
                .unwrap()
                .is_some(),
            "precondition: the chat is showing a loading spinner"
        );

        // The real thing.
        apply_disconnect(&state, &host, Some("link dropped".to_owned()));
        next_tick().await;
        next_tick().await;

        // The user is still standing in their conversation.
        assert!(
            state.viewing_chat.get_untracked(),
            "a dropped connection must not teleport the user out of the chat they \
             were reading"
        );
        assert!(
            state.active_agent.get_untracked().is_some(),
            "the navigation pointer is not server state and must survive the drop"
        );

        // Stale server state is *not* kept around pretending to be authoritative.
        assert!(
            state.agents.get_untracked().is_empty(),
            "the agent snapshot is server state and must be dropped"
        );
        assert!(
            state.agent_loaded.get_untracked().is_empty(),
            "a bootstrap from a dead stream must not still count as loaded"
        );
        assert!(
            state.agent_load_requests.get_untracked().is_empty(),
            "the load latch must clear so the next connection re-loads the agent"
        );

        // And the spinner is gone, replaced by something the user can act on.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-loading-spinner']")
                .unwrap()
                .is_none(),
            "a bootstrap that can never arrive must not still render as loading"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-empty']")
                .unwrap()
                .is_none(),
            "the conversation is not empty — we simply no longer know what is in it. \
             Calling it empty is a lie the user would act on"
        );
        let failed = container
            .query_selector("[data-mobile-test='chat-load-failed']")
            .unwrap()
            .expect("the real disconnect lifecycle must render the terminal recovery");
        assert_eq!(
            failed.get_attribute("role").as_deref(),
            Some("alert"),
            "a screen reader told 'Loading conversation' must be told how it ended"
        );
        let reconnect = container
            .query_selector("[data-mobile-test='chat-load-failed-reconnect']")
            .unwrap()
            .expect("the terminal state must offer the action that fixes it");
        assert!(
            !reconnect.has_attribute("disabled"),
            "the reconnect must be reachable"
        );

        // The in-flight message is still recoverable.
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "a disconnect must never destroy a held submission — that record is the \
             only copy of the user's text"
        );
        // And the truth about the one the user threw away survives too. A disconnect
        // is not teardown: only forgetting the host is, and until then a tool card
        // that discarded its reply must keep saying so rather than reverting to
        // "queued locally".
        assert_eq!(
            state.submission_lifecycle(discarded),
            crate::state::SubmissionLifecycle::Withdrawn(
                crate::state::SubmissionWithdrawal::Discarded
            ),
            "a dropped connection must not resurrect the 'queued locally' lie about a \
             message the user discarded"
        );
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
                connection_instance_id: None,
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

    // ── apply_connection_status tests ──────────────────────────────────

    /// Second `Connected` with no instance id and status already Connected +
    /// host_stream present uses the legacy heuristic: same stream, no re-Hello.
    #[wasm_bindgen_test]
    fn connected_status_replay_does_not_replace_host_stream() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-connected-replay".to_owned());

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            None,
        );
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should have stream");

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            None,
        );
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should still have stream");

        assert_eq!(second_stream, first_stream);
    }

    /// `prepare_frontend_reattach` clears host streams so the next Connected
    /// event creates a new one.  This function is no longer called from
    /// `install_event_listeners`; the test documents its inherent behavior.
    #[wasm_bindgen_test]
    fn frontend_reattach_forces_connected_status_to_refresh_host_stream() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-frontend-reattach".to_owned());

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            None,
        );
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("connected host should have stream");

        prepare_frontend_reattach(&state);
        assert!(
            state.host_stream_untracked(&host).is_none(),
            "reattach must drop the old host stream so the next Connected status sends Hello"
        );

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            None,
        );
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("reattached connected host should have a stream");

        assert_ne!(second_stream, first_stream);
    }

    /// An `IncompatibleProtocol` reject makes the status a sticky
    /// `UpdateRequired` that transport reconnect churn cannot overwrite:
    /// neither `Connecting` nor `Connected` may replace it or re-run the
    /// Connected branch (which would allocate a host stream and re-send Hello,
    /// only to be rejected again — the "spinning forever" bug).
    #[wasm_bindgen_test]
    fn update_required_status_is_sticky_over_transport_reconnect() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-update-required".to_owned());
        let update_required = ConnectionStatus::UpdateRequired {
            host_protocol: 31,
            app_protocol: 30,
            release_version: None,
        };

        // Simulate the outcome of dispatching a Reject { IncompatibleProtocol }.
        state.connection_statuses.update(|m| {
            m.insert(host.clone(), update_required.clone());
        });

        // Transport keeps reconnecting underneath the app-level rejection.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connecting,
            None,
        );
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(9),
        );
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Disconnected {
                reason: "socket dropped".to_owned(),
            },
            None,
        );

        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&update_required),
            "transport reconnect statuses must not overwrite a sticky UpdateRequired",
        );
        assert!(
            state.host_stream_untracked(&host).is_none(),
            "the Connected branch must not run under UpdateRequired: no stream, no re-Hello",
        );
        assert!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .is_none(),
            "no connection instance should be tracked while an update is required",
        );
        // `host_snapshot_pending` must be false so Home renders the actionable
        // error instead of an indefinite loading skeleton.
        state.active_local_host_id.set(Some(host.clone()));
        assert!(
            !state.host_snapshot_pending(),
            "an update-required host must not read as a pending snapshot (no spinner)",
        );
    }

    /// A transport-level host error must NOT overwrite a sticky
    /// `UpdateRequired`: doing so would let the next transport `Connected`
    /// re-send Hello and reopen the reject loop.
    #[wasm_bindgen_test]
    fn host_error_does_not_overwrite_update_required() {
        let state = AppState::new();
        let host = LocalHostId("host-error-vs-update".to_owned());
        let update_required = ConnectionStatus::UpdateRequired {
            host_protocol: 31,
            app_protocol: 30,
            release_version: None,
        };
        state.connection_statuses.update(|m| {
            m.insert(host.clone(), update_required.clone());
        });

        apply_host_error(&state, host.clone(), "MQTT connection dropped".to_owned());

        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&update_required),
            "a host error must not clobber the sticky UpdateRequired",
        );

        // A plain host on the same path is still recorded as an Error.
        let other = LocalHostId("host-plain-error".to_owned());
        apply_host_error(&state, other.clone(), "socket closed".to_owned());
        assert!(matches!(
            state.connection_statuses.get_untracked().get(&other),
            Some(ConnectionStatus::Error(msg)) if msg == "socket closed"
        ));
    }

    /// The real flow: `Connected` allocates a host stream / instance id / sends
    /// Hello, then the host answers with `Reject(IncompatibleProtocol)`.
    /// Dispatching the reject through the actual frame path must (a) make the
    /// status a sticky `UpdateRequired` carrying the host build, (b) tear down
    /// the runtime connection state (stream + instance id), and (c) survive the
    /// transport reconnect churn without re-allocating a stream — the loop is
    /// broken end-to-end, not just on a synthetic status insert.
    #[wasm_bindgen_test]
    fn connected_then_incompatible_reject_clears_runtime_and_is_sticky() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-connected-reject".to_owned());

        // 1. Transport connects: stream + instance id allocated, Hello sent.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(9),
        );
        let stream = state
            .host_stream_untracked(&host)
            .expect("Connected must allocate a host stream");
        assert_eq!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .copied(),
            Some(9),
        );

        // 2. Host answers Hello with an incompatible-protocol reject, on the
        //    same host stream, at seq 0 — dispatched through the real line path.
        let reject = protocol::RejectPayload {
            code: protocol::RejectCode::IncompatibleProtocol,
            message: "protocol 30 is no longer supported".to_owned(),
            server_protocol_version: protocol::PROTOCOL_VERSION + 1,
            server_tyde_version: protocol::TYDE_VERSION,
            release_version: Some(protocol::TydeReleaseVersion::parse("0.8.19-beta.15").unwrap()),
        };
        let envelope =
            protocol::Envelope::from_payload(stream, protocol::FrameKind::Reject, 0, &reject)
                .expect("build reject envelope");
        handle_host_line_event(
            &state,
            bridge::HostLineEvent {
                host_id: host.0.clone(),
                line: serde_json::to_string(&envelope).expect("serialize reject"),
                connection_instance_id: None,
                delivery_id: None,
            },
        );

        // 3a. Sticky UpdateRequired carrying the reject's protocol + build.
        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::UpdateRequired {
                host_protocol: protocol::PROTOCOL_VERSION + 1,
                app_protocol: protocol::PROTOCOL_VERSION,
                release_version: Some(
                    protocol::TydeReleaseVersion::parse("0.8.19-beta.15").unwrap()
                ),
            }),
        );
        // 3b. Runtime connection state torn down.
        assert!(
            state.host_stream_untracked(&host).is_none(),
            "the reject must clear the stale host stream",
        );
        assert!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .is_none(),
            "the reject must clear the stale connection-instance id",
        );

        // 4. Transport keeps reconnecting: no new stream, no re-Hello, status
        //    unchanged — the spinning-forever loop stays broken.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connecting,
            None,
        );
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(10),
        );
        assert!(
            state.host_stream_untracked(&host).is_none(),
            "a post-reject Connected must not re-allocate a stream or re-send Hello",
        );
        assert!(matches!(
            state.connection_statuses.get_untracked().get(&host),
            Some(ConnectionStatus::UpdateRequired { .. })
        ));
    }

    // ── New instance-id lifecycle tests ────────────────────────────────

    /// Same `connection_instance_id` on a second Connected event: no new
    /// host stream, no seq reset — the frontend is already attached to this
    /// exact MQTT connection.
    #[wasm_bindgen_test]
    fn same_instance_id_connected_replay_preserves_stream() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-same-instance".to_owned());

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(7),
        );
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("first Connected should allocate a stream");
        assert_eq!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .copied(),
            Some(7),
            "instance id should be recorded"
        );

        // Replay same instance id (simulates frontend_attached() when the
        // native manager keeps the connection alive).
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(7),
        );
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("stream must survive same-instance replay");

        assert_eq!(
            second_stream, first_stream,
            "same-instance replay must not replace the host stream"
        );
        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Bootstrapping),
        );
    }

    /// A status replay for an already-known connection must never clear
    /// reactive state. When the native side preserves a connection the
    /// frontend receives a Connected replay with the same instance id; the
    /// stream and data must survive.
    #[wasm_bindgen_test]
    fn frontend_attach_alone_does_not_clear_state() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-attach-no-clear".to_owned());

        // Simulate an established connection with some host-scoped data.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(3),
        );
        let established_stream = state
            .host_stream_untracked(&host)
            .expect("established connection must have a stream");

        // Seed a project so we can assert it survives.
        use crate::state::ProjectInfo;
        state.projects.update(|v| {
            v.push(ProjectInfo {
                local_host_id: host.clone(),
                project: protocol::Project {
                    id: protocol::ProjectId("proj-1".to_owned()),
                    name: "Persist Me".to_owned(),
                    source: protocol::ProjectSource::Standalone { roots: Vec::new() },
                    sort_order: 0,
                },
            });
        });

        // Simulate receiving a Connected status replay with the same
        // instance id after telling native that this frontend already knows
        // about the connection.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(3),
        );

        assert_eq!(
            state.host_stream_untracked(&host).as_ref(),
            Some(&established_stream),
            "frontend attach alone must not replace the host stream"
        );
        let projects = state.projects.get_untracked();
        assert!(
            projects.iter().any(|p| p.local_host_id == host),
            "project data must survive a same-instance Connected replay"
        );
    }

    /// A new `connection_instance_id` on a Connected event allocates a new
    /// host stream (so Hello is sent) but does NOT wipe existing reactive
    /// state — that arrives and replaces atomically via host_bootstrap.
    #[wasm_bindgen_test]
    fn new_instance_id_sends_new_hello_without_pre_clearing_state() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-new-instance".to_owned());

        // Establish with instance 1.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(1),
        );
        let first_stream = state
            .host_stream_untracked(&host)
            .expect("first connection must have a stream");

        // Seed a project to verify it survives the new-instance transition.
        use crate::state::ProjectInfo;
        state.projects.update(|v| {
            v.push(ProjectInfo {
                local_host_id: host.clone(),
                project: protocol::Project {
                    id: protocol::ProjectId("proj-x".to_owned()),
                    name: "Keep Me".to_owned(),
                    source: protocol::ProjectSource::Standalone { roots: Vec::new() },
                    sort_order: 0,
                },
            });
        });

        // MQTT reconnect: new instance id.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(2),
        );
        let second_stream = state
            .host_stream_untracked(&host)
            .expect("new-instance Connected must allocate a fresh stream");

        assert_ne!(
            second_stream, first_stream,
            "new instance id must produce a different host stream (new Hello)"
        );
        assert_eq!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .copied(),
            Some(2),
            "tracked instance id must advance to the new value"
        );
        // Data must NOT have been pre-cleared; bootstrap will replace it.
        let projects = state.projects.get_untracked();
        assert!(
            projects.iter().any(|p| p.local_host_id == host),
            "existing data must survive the new-instance transition; bootstrap clears it"
        );
    }

    #[wasm_bindgen_test]
    fn same_instance_replay_before_new_bootstrap_stays_bootstrapping() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-replay-before-bootstrap".to_owned());
        let old_stream = protocol::StreamPath("/host/old-bootstrap".to_owned());

        state.host_streams.update(|m| {
            m.insert(host.clone(), old_stream.clone());
        });
        state.bootstrapped_host_streams.update(|m| {
            m.insert(host.clone(), old_stream);
        });
        state.host_settings_by_host.update(|m| {
            m.insert(host.clone(), empty_host_settings());
        });
        state.active_connection_instance_ids.update(|m| {
            m.insert(host.clone(), 1);
        });
        state.connection_statuses.update(|m| {
            m.insert(host.clone(), ConnectionStatus::Connected);
        });

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(2),
        );
        let new_stream = state
            .host_stream_untracked(&host)
            .expect("new instance must allocate a stream");
        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Bootstrapping),
            "new instance must wait for its own HostBootstrap"
        );
        assert!(
            !state.host_bootstrap_applied_for_current_stream_untracked(&host),
            "old HostBootstrap state must not match the new stream"
        );

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(2),
        );

        assert_eq!(
            state.host_stream_untracked(&host),
            Some(new_stream),
            "duplicate same-instance Connected must preserve the active stream"
        );
        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Bootstrapping),
            "duplicate same-instance Connected must not use stale HostSettings as proof of bootstrap"
        );
    }

    /// `Connecting` after a previous Connected keeps all reactive state
    /// visible so the UI shows stale projections during reconnect.
    #[wasm_bindgen_test]
    fn connecting_after_connected_preserves_stale_state() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-connecting-stale".to_owned());

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(5),
        );
        let stream = state
            .host_stream_untracked(&host)
            .expect("connected host must have a stream");

        // Seed a project.
        use crate::state::ProjectInfo;
        state.projects.update(|v| {
            v.push(ProjectInfo {
                local_host_id: host.clone(),
                project: protocol::Project {
                    id: protocol::ProjectId("proj-stale".to_owned()),
                    name: "Stale But Visible".to_owned(),
                    source: protocol::ProjectSource::Standalone { roots: Vec::new() },
                    sort_order: 0,
                },
            });
        });

        // Simulate the MQTT drop: manager emits Connecting while retrying.
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connecting,
            None,
        );

        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Connecting),
            "status must reflect the reconnecting state"
        );
        assert_eq!(
            state.host_stream_untracked(&host).as_ref(),
            Some(&stream),
            "host stream must survive a Connecting transition"
        );
        let projects = state.projects.get_untracked();
        assert!(
            projects.iter().any(|p| p.local_host_id == host),
            "project data must remain visible while reconnecting"
        );
        assert_eq!(
            state
                .active_connection_instance_ids
                .get_untracked()
                .get(&host)
                .copied(),
            Some(5),
            "tracked instance id must survive a Connecting transition"
        );
    }

    /// Terminal `Disconnected` and `Failed` statuses clear all per-host
    /// reactive state exactly as they did before.
    #[wasm_bindgen_test]
    fn terminal_disconnected_and_failed_clear_state() {
        let _send_guard = bridge::test_clean_sends();
        // ── Disconnected ─────────────────────────────────────────────
        {
            let state = AppState::new();
            let host = LocalHostId("host-term-disconnected".to_owned());

            apply_connection_status(
                &state,
                host.clone(),
                PairedHostConnectionStatus::Connected,
                Some(9),
            );
            use crate::state::ProjectInfo;
            state.projects.update(|v| {
                v.push(ProjectInfo {
                    local_host_id: host.clone(),
                    project: protocol::Project {
                        id: protocol::ProjectId("p".to_owned()),
                        name: "Gone".to_owned(),
                        source: protocol::ProjectSource::Standalone { roots: Vec::new() },
                        sort_order: 0,
                    },
                });
            });

            apply_connection_status(
                &state,
                host.clone(),
                PairedHostConnectionStatus::Disconnected {
                    reason: "user disconnect".to_owned(),
                },
                None,
            );

            assert!(
                state.host_stream_untracked(&host).is_none(),
                "Disconnected must clear the host stream"
            );
            assert!(
                state.projects.get_untracked().is_empty(),
                "Disconnected must clear projects"
            );
            assert!(
                state
                    .active_connection_instance_ids
                    .get_untracked()
                    .get(&host)
                    .is_none(),
                "Disconnected must clear the tracked instance id"
            );
        }

        // ── Failed ───────────────────────────────────────────────────
        {
            let state = AppState::new();
            let host = LocalHostId("host-term-failed".to_owned());

            apply_connection_status(
                &state,
                host.clone(),
                PairedHostConnectionStatus::Connected,
                Some(11),
            );
            use crate::state::ProjectInfo;
            state.projects.update(|v| {
                v.push(ProjectInfo {
                    local_host_id: host.clone(),
                    project: protocol::Project {
                        id: protocol::ProjectId("q".to_owned()),
                        name: "Also Gone".to_owned(),
                        source: protocol::ProjectSource::Standalone { roots: Vec::new() },
                        sort_order: 0,
                    },
                });
            });

            apply_connection_status(
                &state,
                host.clone(),
                PairedHostConnectionStatus::Failed {
                    code: protocol::MobileAccessErrorCode::TransportFailed,
                    message: "fatal error".to_owned(),
                },
                None,
            );

            assert!(
                state.host_stream_untracked(&host).is_none(),
                "Failed must clear the host stream"
            );
            assert!(
                state.projects.get_untracked().is_empty(),
                "Failed must clear projects"
            );
            assert!(
                state
                    .active_connection_instance_ids
                    .get_untracked()
                    .get(&host)
                    .is_none(),
                "Failed must clear the tracked instance id"
            );
        }
    }

    #[wasm_bindgen_test]
    fn host_line_delivery_ids_are_deduplicated() {
        let host = LocalHostId("host-line-dedupe".to_owned());

        assert!(mark_host_line_seen(&host, 42));
        assert!(!mark_host_line_seen(&host, 42));
        assert!(mark_host_line_seen(&host, 43));
    }

    #[wasm_bindgen_test]
    fn stale_host_lines_from_previous_connection_are_ignored() {
        let _send_guard = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("host-stale-line".to_owned());

        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(1),
        );
        apply_connection_status(
            &state,
            host.clone(),
            PairedHostConnectionStatus::Connected,
            Some(2),
        );

        handle_host_line_event(
            &state,
            bridge::HostLineEvent {
                host_id: host.0.clone(),
                line: r#"{"not":"an envelope from the active connection"}"#.to_owned(),
                connection_instance_id: Some(1),
                delivery_id: Some(12),
            },
        );

        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Bootstrapping),
            "stale line must not poison the active connection"
        );
        assert!(
            state.mobile_shell_error.get_untracked().is_none(),
            "stale line must be dropped before parse/protocol validation"
        );
    }

    /// A boot `AuthFailed` callback must not discard a still-valid pending
    /// managed QR: it rides into the `ServiceAuth` screen so `ServiceAuthCard`
    /// renders the sign-in buttons and one tap retries — no re-scan.
    #[wasm_bindgen_test]
    fn boot_auth_failed_with_pending_managed_qr_keeps_service_auth_screen() {
        let screen = boot_pairing_screen(
            "tyde-pair://v2?offer".to_owned(),
            PairingOffer::ManagedService {
                host_label: "Mike's MacBook Pro".to_owned(),
            },
            Some(MobileServiceAuthState::AuthFailed {
                message: "Tyggs sign-in failed: oauth_no_linked_account".to_owned(),
            }),
        );
        match screen {
            PairingScreen::ServiceAuth {
                qr_uri,
                host_label,
                auth: MobileServiceAuthState::AuthFailed { message },
            } => {
                assert_eq!(
                    qr_uri, "tyde-pair://v2?offer",
                    "the pending QR must be kept"
                );
                assert_eq!(host_label, "Mike's MacBook Pro");
                assert!(message.contains("oauth_no_linked_account"), "{message}");
            }
            other => panic!("expected ServiceAuth carrying AuthFailed, got {other:?}"),
        }

        // Without a callback the managed arm starts from Idle, as before.
        let screen = boot_pairing_screen(
            "tyde-pair://v2?offer".to_owned(),
            PairingOffer::ManagedService {
                host_label: "Mike's MacBook Pro".to_owned(),
            },
            None,
        );
        assert!(matches!(
            screen,
            PairingScreen::ServiceAuth {
                auth: MobileServiceAuthState::Idle,
                ..
            }
        ));
    }

    #[wasm_bindgen_test]
    fn boot_handoff_without_pending_qr_keeps_success_out_of_sign_in_flow() {
        let state = AppState::new();
        apply_boot_auth_without_pending(
            &state,
            MobileServiceAuthState::Authenticated { expires_at_ms: 42 },
        );

        assert_eq!(
            state.app_mode.get_untracked(),
            AppMode::Onboarding,
            "successful boot auth must return to onboarding rather than another sign-in screen"
        );

        apply_boot_auth_without_pending(
            &state,
            MobileServiceAuthState::AuthFailed {
                message: "Tyggs sign-in failed: oauth_no_linked_account".to_owned(),
            },
        );
        assert!(matches!(
            state.app_mode.get_untracked(),
            AppMode::Pairing(PairingScreen::Failed { message })
                if message.contains("oauth_no_linked_account")
        ));

        apply_boot_auth_without_pending(
            &state,
            MobileServiceAuthState::PassRequired {
                message: "A Tyggs Pass is required.".to_owned(),
                paywall_url: "https://tyggs.com/pass/checkout".to_owned(),
            },
        );
        assert!(matches!(
            state.app_mode.get_untracked(),
            AppMode::Pairing(PairingScreen::ServiceAuthStatus {
                auth: MobileServiceAuthState::PassRequired { paywall_url, .. },
            }) if paywall_url == "https://tyggs.com/pass/checkout"
        ));

        apply_boot_auth_without_pending(
            &state,
            MobileServiceAuthState::ServiceUnavailable {
                message: "Try again shortly.".to_owned(),
                retryable: true,
            },
        );
        assert!(matches!(
            state.app_mode.get_untracked(),
            AppMode::Pairing(PairingScreen::ServiceAuthStatus {
                auth: MobileServiceAuthState::ServiceUnavailable {
                    retryable: true,
                    ..
                },
            })
        ));
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
