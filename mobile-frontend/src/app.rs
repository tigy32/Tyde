#[cfg(target_arch = "wasm32")]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

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
use protocol::{FrameKind, HeartbeatPayload};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_arch = "wasm32")]
const BACKGROUND_RECONNECT_AFTER: Duration = Duration::from_secs(15);

thread_local! {
    static SEEN_HOST_LINES: RefCell<HashSet<(LocalHostId, u64)>> =
        RefCell::new(HashSet::new());
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static PAGE_HIDDEN_SINCE_MS: Cell<Option<u64>> = const { Cell::new(None) };
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    crate::components::settings_view::restore_appearance(&state);
    provide_context(state.clone());

    crate::bundle::restore_selection(&state);
    spawn_local(install_event_listeners(state.clone()));
    install_page_visibility_recovery(state.clone());
    install_app_mode_effect(state.clone());
    spawn_boot_pairing_handoff(state.clone());
    spawn_heartbeat_loop(state.clone());

    view! { <AppSurface /> }
}

#[cfg(all(feature = "ui-fixtures", debug_assertions))]
#[component]
pub fn FixtureApp() -> impl IntoView {
    let state = AppState::new();
    crate::fixtures::seed_state(&state);
    provide_context(state);

    view! { <AppSurface /> }
}

#[component]
pub(crate) fn AppSurface() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    crate::bundle::install(state.clone());
    crate::native::install(state.clone());
    mirror_theme_to_document(state.clone());
    let root = NodeRef::<leptos::html::Div>::new();
    crate::shell::install(root);
    let native_layout = Memo::new(move |previous: Option<&(bool, bool)>| {
        let mode = state.app_mode.get();
        let pairing = matches!(mode, AppMode::Pairing(_));
        let workspace = crate::native::is_native()
            && match mode {
                AppMode::Workspace | AppMode::HostPicker => true,
                AppMode::Pairing(_) => match previous {
                    Some((workspace, true)) => *workspace,
                    _ => state.active_local_host_id.get_untracked().is_some(),
                },
                AppMode::Onboarding => false,
            };
        (workspace, pairing)
    });
    let native_workspace = Memo::new(move |_| native_layout.get().0);
    view! {
        <div
            class="mobile-app tws-shell"
            node_ref=root
            data-theme=move || state.theme.get()
            data-native-host=crate::native::is_native().then_some("")
            data-mobile-fixture={
                #[cfg(all(feature = "ui-fixtures", debug_assertions))]
                { crate::fixtures::fixture_name() }
                #[cfg(not(all(feature = "ui-fixtures", debug_assertions)))]
                { String::new() }
            }
        >
            <header class="shell-empty-header" hidden />
            <footer class="shell-empty-bottom" hidden />
            // Mounted in every app mode so a paste-failed-during-pairing or
            // listener-registration failure stays visible. (Phase C HIGH 4.)
            <components::MobileShellErrorBanner />
            {crate::native::is_native().then(|| view! { <components::NativeAuthRecoverySheet /> })}
            {move || {
                // A Hosts/Back round trip must not dispose component-owned drafts.
                if native_workspace.get() {
                    return view! { <WorkspaceShell /> }.into_any();
                }
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
                    AppMode::HostPicker => view! { <components::PairedHostsPicker /> }.into_any(),
                }
            }}
        </div>
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundRecovery {
    Probe,
    Reconnect,
}

#[cfg(target_arch = "wasm32")]
fn foreground_recovery(background_ms: u64) -> ForegroundRecovery {
    if background_ms >= BACKGROUND_RECONNECT_AFTER.as_millis() as u64 {
        ForegroundRecovery::Reconnect
    } else {
        ForegroundRecovery::Probe
    }
}

#[cfg(target_arch = "wasm32")]
fn install_page_visibility_recovery(state: AppState) {
    if crate::native::is_native() {
        return;
    }
    use wasm_bindgen::{JsCast, closure::Closure};

    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        report_shell_error(
            &state,
            MobileAccessErrorCode::Internal,
            "Failed to install foreground recovery: browser document is unavailable.".to_owned(),
        );
        return;
    };
    let document_for_callback = document.clone();
    let state_for_callback = state.clone();
    let callback = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        let hidden = js_sys::Reflect::get(document_for_callback.as_ref(), &"hidden".into())
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if hidden {
            page_background();
            return;
        }

        page_foreground(state_for_callback.clone());
    });
    if let Err(error) = document
        .add_event_listener_with_callback("visibilitychange", callback.as_ref().unchecked_ref())
    {
        report_shell_error(
            &state,
            MobileAccessErrorCode::Internal,
            format!("Failed to install foreground recovery: {error:?}"),
        );
        return;
    }
    callback.forget();
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn page_background() {
    PAGE_HIDDEN_SINCE_MS.with(|hidden_since| hidden_since.set(Some(unix_time_ms())));
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn page_background() {}

#[cfg(target_arch = "wasm32")]
pub(crate) fn page_foreground(state: AppState) {
    if let Some(hidden_since) = PAGE_HIDDEN_SINCE_MS.with(|hidden_since| hidden_since.take()) {
        spawn_local(async move {
            recover_after_foreground(&state, unix_time_ms().saturating_sub(hidden_since)).await;
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn page_foreground(_state: AppState) {}

#[cfg(not(target_arch = "wasm32"))]
fn install_page_visibility_recovery(_state: AppState) {}

#[cfg(target_arch = "wasm32")]
async fn recover_after_foreground(state: &AppState, background_ms: u64) {
    let Some(host) = state.active_local_host_id.get_untracked() else {
        return;
    };
    match foreground_recovery(background_ms) {
        ForegroundRecovery::Probe => heartbeat_tick(state).await,
        ForegroundRecovery::Reconnect => {
            state.heartbeat_pending_since_by_host.update(|pending| {
                pending.remove(&host);
            });
            state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
                round_trips.remove(&host);
            });
            let reason = bridge::ConnectionInvalidation::ForegroundResume { background_ms };
            if bridge::invalidate_host_connection(&host, reason).is_err()
                && let Err(error) = bridge::connect_paired_host(&host).await
            {
                log::warn!("foreground recovery could not reconnect host {host}: {error}");
            }
        }
    }
}

fn spawn_heartbeat_loop(state: AppState) {
    spawn_local(async move {
        loop {
            sleep(HEARTBEAT_INTERVAL).await;
            heartbeat_tick(&state).await;
        }
    });
}

async fn heartbeat_tick(state: &AppState) {
    let hosts = state.connection_statuses.with_untracked(|statuses| {
        statuses
            .iter()
            .filter_map(|(host, status)| {
                matches!(status, ConnectionStatus::Connected).then_some(host.clone())
            })
            .collect::<Vec<_>>()
    });

    for host in hosts {
        let now = unix_time_ms();
        let pending_since = state
            .heartbeat_pending_since_by_host
            .with_untracked(|pending| pending.get(&host).copied());
        if let Some(pending_since) = pending_since {
            let elapsed_ms = now.saturating_sub(pending_since);
            if elapsed_ms >= HEARTBEAT_TIMEOUT.as_millis() as u64 {
                state.heartbeat_pending_since_by_host.update(|pending| {
                    pending.remove(&host);
                });
                state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
                    round_trips.remove(&host);
                });
                if let Err(error) = bridge::invalidate_host_connection(
                    &host,
                    bridge::ConnectionInvalidation::HeartbeatTimeout { elapsed_ms },
                ) {
                    log::warn!("heartbeat timeout could not invalidate host {host}: {error}");
                }
            }
            continue;
        }

        let Some(stream) = state.host_stream_untracked(&host) else {
            continue;
        };
        state.heartbeat_pending_since_by_host.update(|pending| {
            pending.insert(host.clone(), now);
        });
        let payload = HeartbeatPayload {
            client_sent_at_ms: now,
        };
        if let Err(error) = send_frame(&host, stream, FrameKind::Heartbeat, &payload).await {
            state.heartbeat_pending_since_by_host.update(|pending| {
                pending.remove(&host);
            });
            log::warn!("failed to send heartbeat to host {host}: {error}");
        }
    }
}

fn unix_time_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    return js_sys::Date::now().max(0.0) as u64;

    #[cfg(not(target_arch = "wasm32"))]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep(duration: Duration) {
    tyde_time::sleep(duration).await;
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
    if crate::native::is_native() {
        return view! { <NativeWorkspace /> }.into_any();
    }
    view! {
        <components::ConnectionBanner />
        <crate::voice::MobileVoiceRuntime />
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
                        MobileTab::Settings => view! { <components::SettingsView /> }.into_any(),
                    }
                }
            }}
        </div>
        <Show when=move || !state.viewing_chat.get()>
            <footer class="shell-bottom tws-bottom">
                <components::BottomNav />
            </footer>
        </Show>
        // Outside the content column so the sheet covers the tab dock too: a
        // modal the user can navigate out from underneath is not modal.
        <components::ProjectPicker />
        <components::SessionSettingsSheet />
    }
    .into_any()
}

#[component]
fn RetainedNativePane(tab: MobileTab, children: ChildrenFn) -> impl IntoView {
    let state = expect_context::<AppState>();
    let visited = RwSignal::new(false);
    let visible_state = state.clone();
    let visible = Signal::derive(move || {
        let selected = visible_state.active_tab.get();
        let chat = crate::native::chat_visible(&visible_state);
        let split = crate::native::sidebar_visible(&visible_state);
        visible_state.app_mode.get() == AppMode::Workspace
            && ((selected == tab && !chat) || (tab == MobileTab::Agents && split))
    });
    Effect::new(move |_| {
        if visible.get() {
            visited.set(true);
        }
    });
    view! {
        <div class="native-pane" class:native-agents-pane=tab == MobileTab::Agents hidden=move || !visible.get()
            data-native-shell-active=move || (visible.get() && !crate::native::chat_visible(&state)).then_some("")>
            <Show when=move || visited.get()>{children()}</Show>
        </div>
    }
}

#[component]
fn NativeWorkspace() -> impl IntoView {
    let state = expect_context::<AppState>();
    let chat_visited = RwSignal::new(false);
    let visit_state = state.clone();
    Effect::new(move |_| {
        if visit_state.viewing_chat.get() {
            chat_visited.set(true);
        }
    });
    let split_state = state.clone();
    let visible_state = state.clone();
    view! {
        <div class="native-pane native-workspace-status" hidden=move || matches!(state.app_mode.get(), AppMode::Pairing(_))>
            <components::ConnectionBanner />
            <components::PendingSubmissions />
        </div>
        <crate::voice::MobileVoiceRuntime />
        <div class="mobile-content native-workspace"
            class:native-pairing=move || matches!(state.app_mode.get(), AppMode::Pairing(_))
            class:native-split=move || {
            crate::native::sidebar_visible(&split_state)
        }>
            <RetainedNativePane tab=MobileTab::Home><components::HomeView /></RetainedNativePane>
            <RetainedNativePane tab=MobileTab::Agents><components::AgentsView /></RetainedNativePane>
            <RetainedNativePane tab=MobileTab::Sessions><components::SessionsView /></RetainedNativePane>
            <RetainedNativePane tab=MobileTab::Settings><components::SettingsView /></RetainedNativePane>
            <div class="native-pane" hidden=move || !matches!(state.app_mode.get(), AppMode::Pairing(_))
                data-native-shell-active=move || matches!(state.app_mode.get(), AppMode::Pairing(_)).then_some("")>
                {move || match state.app_mode.get() {
                    AppMode::Pairing(screen) => view! { <components::PairingFlow screen=screen /> }.into_any(),
                    _ => ().into_any(),
                }}
            </div>
            <div class="native-pane" hidden=move || state.app_mode.get() != AppMode::HostPicker
                data-native-shell-active=move || (state.app_mode.get() == AppMode::HostPicker).then_some("")>
                <Show when=move || state.app_mode.get() == AppMode::HostPicker>
                    <components::PairedHostsPicker />
                </Show>
            </div>
            <div class="native-pane native-chat-pane" hidden={let state = state.clone(); move || !crate::native::chat_visible(&state)}
                data-native-shell-active=move || crate::native::chat_visible(&visible_state).then_some("")>
                <Show when=move || chat_visited.get()>
                    <For each=move || vec![(state.active_agent.get().map(|active| active.as_agent_ref()), state.chat_draft_id.get())] key=Clone::clone let:_chat>
                        <components::ChatView />
                    </For>
                </Show>
            </div>
        </div>
        <components::ProjectPicker />
        <components::SessionSettingsSheet />
    }
}

/// Mirrors the theme onto `<html>` so the palette exists on the document
/// element as well as on the app shell.
///
/// The shell cannot cover every pixel: iOS paints the canvas (the root
/// element's background) across the whole window, including the safe-area
/// bands and any strip a mis-measured viewport leaves below the shell. In
/// production the app mounts into the loader's page, which carries no
/// `data-theme`, so a rule on `html` would otherwise resolve `var(--bg-base)`
/// to nothing and leave the loader's own colour showing at the edges.
fn mirror_theme_to_document(state: AppState) {
    Effect::new(move |_| {
        let theme = state.theme.get();
        let Some(root) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.document_element())
        else {
            return;
        };
        let _ = root.set_attribute("data-theme", &theme);
    });
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
            AppMode::Workspace | AppMode::HostPicker => {
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
    state.boot_handoff_complete.set(false);
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
        state.boot_handoff_complete.set(true);
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
        PairingOffer::SelfHosted { host_label } => {
            PairingScreen::SelfHostedConfirm { qr_uri, host_label }
        }
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

fn report_shell_error(state: &AppState, code: MobileAccessErrorCode, message: String) {
    state
        .mobile_shell_error
        .set(Some(MobileShellError { code, message }));
}

async fn install_event_listeners(state: AppState) {
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

    register_listener(
        &state,
        "submission-transport-outcome",
        listen_submission_outcomes(&state).await,
    );

    // Resolve selection before transport status replay can start any handshake.
    match bridge::list_paired_hosts().await {
        Ok(hosts) => apply_paired_hosts_list(&state, hosts),
        Err(error) => {
            report_shell_error(
                &state,
                MobileAccessErrorCode::Internal,
                format!("Failed to load paired hosts: {error}"),
            );
            return;
        }
    }

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
            report_shell_error(state, MobileAccessErrorCode::TransportFailed, message);
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

/// Applies each transport outcome the bridge reports to its held submission.
pub(crate) async fn listen_submission_outcomes(
    state: &AppState,
) -> Result<bridge::UnlistenHandle, String> {
    let state = state.clone();
    bridge::listen_submission_transport_outcome(move |event| {
        log::info!(
            "submission_transport_outcome host={} connection_instance_id={} \
             local_submission_id={} outcome={:?}",
            event.local_host_id,
            event.connection_instance_id,
            event.local_submission_id.0,
            event.outcome
        );
        state.apply_submission_outcome(
            event.local_submission_id,
            event.connection_instance_id,
            event.outcome,
        );
    })
    .await
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
    if state.active_local_host_id.get_untracked().is_none()
        && state.paired_hosts.get_untracked().len() == 1
    {
        state.active_local_host_id.set(
            state
                .paired_hosts
                .get_untracked()
                .first()
                .map(|host| host.local_host_id.clone()),
        );
    }
}

pub(crate) fn apply_connection_status(
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
                drain_pending_host_lines(state.clone());
                return;
            }

            // Statuses owed to a replaced protocol session went to its socket.
            if state.host_stream_untracked(&host).is_some() {
                state.disconnect_human_admissions(&host);
            }
            state.connection_statuses.update(|m| {
                m.insert(host.clone(), ConnectionStatus::Bootstrapping);
            });
            state.bootstrapped_host_streams.update(|m| {
                m.remove(&host);
            });
            state.host_releases.update(|m| {
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
                let hello_result = send_frame(
                    &host_for_hello,
                    stream.clone(),
                    protocol::FrameKind::Hello,
                    &protocol::HelloPayload {
                        protocol_version: protocol::PROTOCOL_VERSION,
                        tyde_version: protocol::TYDE_VERSION,
                        client_name: "tyde-mobile".to_string(),
                        platform: "ios".to_string(),
                    },
                )
                .await;
                if let Err(error) = hello_result {
                    let message = format!("failed to send hello to {host_for_hello}: {error}");
                    log::error!("{message}");
                    report_shell_error(
                        &state_for_hello_error,
                        MobileAccessErrorCode::TransportFailed,
                        message,
                    );
                    return;
                }
                let now = unix_time_ms();
                state_for_hello_error
                    .heartbeat_pending_since_by_host
                    .update(|pending| {
                        pending.insert(host_for_hello.clone(), now);
                    });
                if let Err(error) = send_frame(
                    &host_for_hello,
                    stream,
                    FrameKind::Heartbeat,
                    &HeartbeatPayload {
                        client_sent_at_ms: now,
                    },
                )
                .await
                {
                    state_for_hello_error
                        .heartbeat_pending_since_by_host
                        .update(|pending| {
                            pending.remove(&host_for_hello);
                        });
                    log::warn!("failed to send initial heartbeat to {host_for_hello}: {error}");
                }
            });
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
            state.host_releases.update(|m| {
                m.remove(&host);
            });
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
    state.disconnect_human_admissions(host);
    if state
        .native_chat_destination
        .get_untracked()
        .as_ref()
        .is_some_and(|target| target.local_host_id == *host)
    {
        state.native_chat_destination.set(None);
    }
    // A terminal rejection remains the sticky handshake verdict, even when
    // that rejected socket closes. A Welcome is authority only while live.
    state.host_releases.update(|m| {
        if matches!(
            m.get(host),
            Some(crate::bundle::HostRelease::Welcome { .. })
        ) {
            m.remove(host);
        }
    });
    crate::voice::transport_lost(state, host);
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
    state.heartbeat_pending_since_by_host.update(|m| {
        m.remove(host);
    });
    state.heartbeat_round_trip_ms_by_host.update(|m| {
        m.remove(host);
    });
    state.host_settings_by_host.update(|m| {
        m.remove(host);
    });
    state.voice_capabilities_by_host.update(|m| {
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
    state.git_status.update(|m| {
        m.retain(|(h, _), _| h != host);
    });
    state.chat_messages.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.chat_message_index.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.compaction_markers.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.compaction_marker_ids.update(|m| {
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
    state.agent_awaiting_user.update(|m| {
        m.retain(|k| k.local_host_id != *host);
    });
    state.transient_events.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_session_settings.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.agent_session_schemas.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    // A completed legacy replacement is retained while the old agent is removed
    // inside one connection so its redirect remains actionable. A disconnect is
    // the authoritative boundary: legacy replacement state is not bootstrapped,
    // and retaining it would expose a redirect minted by a dead host snapshot.
    state.agent_compactions.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.context_compaction_operations.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.context_compaction_capabilities.update(|m| {
        m.retain(|k, _| k.local_host_id != *host);
    });
    state.forget_host_terminal_context_compaction_operations(host);
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

    #[wasm_bindgen::prelude::wasm_bindgen(module = "/src/bridge/loader-test.js")]
    extern "C" {
        #[wasm_bindgen(js_name = install)]
        fn install_loader_boundary();
        #[wasm_bindgen(js_name = finish)]
        fn finish_loader_prepare(index: u32, reason: &str);
        #[wasm_bindgen(js_name = preparations)]
        fn loader_preparations() -> u32;
        #[wasm_bindgen(js_name = reloads)]
        fn loader_reloads() -> u32;
        #[wasm_bindgen(js_name = ownsTarget)]
        fn loader_owns_target(host: &str, version: &str) -> bool;
        #[wasm_bindgen(js_name = noTarget)]
        fn loader_no_target() -> bool;
        #[wasm_bindgen(js_name = restore)]
        fn restore_loader_boundary();
    }

    #[wasm_bindgen_test]
    async fn native_tabs_retain_mounted_draft_selection_and_search_without_web_navigation() {
        use mobile_shell_types::native::{NATIVE_BRIDGE_VERSION, NativeIntent};
        crate::components::test_styles::ensure_styles_loaded();
        let _sends = bridge::test_capture_sends();
        let window = web_sys::window().unwrap();
        let native = js_sys::Object::new();
        js_sys::Reflect::set(&native, &"version".into(), &NATIVE_BRIDGE_VERSION.into()).unwrap();
        let post = js_sys::Function::new_with_args(
            "endpoint,payload",
            "if (endpoint === 'presentation') this.presentations.push(JSON.parse(payload)); return Promise.resolve(null)",
        );
        js_sys::Reflect::set(&native, &"post".into(), &post).unwrap();
        js_sys::Reflect::set(&native, &"presentations".into(), &js_sys::Array::new()).unwrap();
        install_loader_boundary();
        js_sys::Reflect::set(&window, &"__tydeNativeHost".into(), &native).unwrap();
        let state = AppState::new();
        let host = fixture_host("native-navigation-host", "Native host");
        let host_id = host.local_host_id.clone();
        let mounted_state = state.clone();
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted_state);
            view! { <AppSurface /> }
        });
        // Wasm panics do not unwind. Restore the platform boundary before reporting failures.
        let result: Result<(), String> = async {
            let check = |condition: bool, message: &str| {
                if condition { Ok(()) } else { Err(message.to_owned()) }
            };
            let find = |selector: &str| -> Result<web_sys::Element, String> {
                container.query_selector(selector)
                    .map_err(|_| "Invalid navigation test selector".to_owned())?
                    .ok_or_else(|| format!("Missing native navigation control: {selector}"))
            };
            let projected = || -> Result<mobile_shell_types::native::NativePresentation, String> {
                let values = js_sys::Reflect::get(&native, &"presentations".into())
                    .map_err(|_| "Native presentations are unavailable")?;
                let values = js_sys::Array::from(&values);
                serde_wasm_bindgen::from_value(values.get(values.length().saturating_sub(1)))
                    .map_err(|_| "Native presentation is invalid".into())
            };
            let navigate = |intent: NativeIntent| -> Result<(), String> {
                let options = web_sys::CustomEventInit::new();
                options.set_detail(&serde_wasm_bindgen::to_value(&intent).map_err(|_| "Invalid native intent")?);
                let event = web_sys::CustomEvent::new_with_event_init_dict("tyde-native-intent", &options)
                    .map_err(|_| "Could not create native intent")?;
                window.dispatch_event(&event).map_err(|_| "Could not dispatch native intent")?;
                Ok(())
            };
            install_page_visibility_recovery(state.clone());
            PAGE_HIDDEN_SINCE_MS.with(|hidden| hidden.set(None));
            js_sys::Function::new_no_args(r#"
                const old = Object.getOwnPropertyDescriptor(document, 'hidden');
                try {
                    Object.defineProperty(document, 'hidden', {configurable:true, value:true});
                    document.dispatchEvent(new Event('visibilitychange'));
                } finally {
                    if (old) Object.defineProperty(document, 'hidden', old);
                    else delete document.hidden;
                }
            "#).call0(&wasm_bindgen::JsValue::NULL).unwrap();
            check(PAGE_HIDDEN_SINCE_MS.with(|hidden| hidden.get().is_none()),
                "Document visibility must not be a second native lifecycle producer")?;
            navigate(NativeIntent::Background)?;
            check(PAGE_HIDDEN_SINCE_MS.with(|hidden| hidden.get().is_some()),
                "UIKit background owns native connection recovery")?;
            navigate(NativeIntent::Foreground)?;
            check(PAGE_HIDDEN_SINCE_MS.with(|hidden| hidden.get().is_none()),
                "UIKit foreground consumes the native recovery interval")?;
            next_tick().await;
            check(container.query_selector("[data-mobile-test='bottom-nav']").unwrap().is_none(),
                "Only native packaging suppresses web global navigation")?;
            check(!projected()?.workspace, "Onboarding must not expose native workspace navigation")?;
            let onboarding = find(".onboarding-view .shell-flow")?;
            check(onboarding.get_bounding_client_rect().width() <= 512.0,
                "Large-screen native onboarding stays bounded")?;
            let paste = container.query_selector_all(".onboarding-actions button").unwrap().item(1).unwrap();
            check(paste.text_content().unwrap_or_default().trim() == "Paste link", "Native pairing uses concise link terminology")?;
            let instructions = find(".native-onboarding-steps")?;
            let scan = find(".onboarding-actions button")?;
            check(scan.get_bounding_client_rect().top() - instructions.get_bounding_client_rect().bottom() >= 24.0,
                "Primary action must be separated from onboarding guidance")?;
            // Source21 iPad39 rendered edge-to-edge picker/link actions and two focus rims.
            // Keep the same real screens/controls while comparing native styling with browser CSS.
            let app = find(".mobile-app")?;
            for width in [320, 390, 820, 1180] {
                app.set_attribute("style", &format!("position:relative;width:{width}px;height:700px")).unwrap();
                state.app_mode.set(AppMode::Workspace);
                state.paired_hosts.set(vec![host.clone()]);
                next_tick().await;
                let picker = find(".picker-view .shell-flow")?;
                let action = find(".picker-pair-another")?;
                let picker_width = picker.get_bounding_client_rect().width();
                check(picker_width > 0.0 && picker_width <= 512.0 && picker_width <= f64::from(width),
                    "Native host picker keeps a compact-responsive bounded column")?;
                check((picker.get_bounding_client_rect().left() + picker_width / 2.0
                    - (app.get_bounding_client_rect().left() + f64::from(width) / 2.0)).abs() < 1.0,
                    "Native host picker content is balanced around its window center")?;
                check(action.get_bounding_client_rect().width() <= picker_width
                    && action.get_bounding_client_rect().height() >= 44.0,
                    "Host pairing actions stay within the column and remain touch usable")?;
                state.app_mode.set(AppMode::Pairing(crate::state::PairingScreen::ManualPaste));
                next_tick().await;
                let flow = find(".pairing-view .shell-flow")?;
                let field = find("textarea[aria-label='Pairing link']")?;
                let button = find(".pairing-view button.ui-button-primary")?;
                let computed = |element: &web_sys::Element, property: &str| {
                    window.get_computed_style(element).unwrap().unwrap().get_property_value(property).unwrap()
                };
                app.remove_attribute("data-native-host").unwrap();
                next_tick().await;
                let browser_width = flow.get_bounding_client_rect().width();
                let browser_font = computed(&field, "font");
                app.set_attribute("data-native-host", "").unwrap();
                next_tick().await;
                let native_width = flow.get_bounding_client_rect().width();
                check(native_width > 0.0 && native_width <= 512.0
                    && (width < 820 || native_width < browser_width),
                    "Native pairing must bound the same full-width layout observed on the old browser-style iPad screen")?;
                check((flow.get_bounding_client_rect().left() + native_width / 2.0
                    - (app.get_bounding_client_rect().left() + f64::from(width) / 2.0)).abs() < 1.0,
                    "Native pairing shares the centered onboarding/host-picker column")?;
                check(computed(&field, "font") == browser_font,
                    "Native pairing preserves existing theme font and size roles")?;
                let before_focus = computed(&field, "box-shadow");
                field.unchecked_ref::<HtmlElement>().focus().unwrap();
                next_tick().await;
                // CSS keeps computed width independent of outline-style:none; assert paint.
                check(computed(&field, "outline-style") == "none" && computed(&field, "box-shadow") != before_focus,
                    "Native pairing editing has one visible contiguous focus rim, not a second offset outline")?;
                check(button.get_bounding_client_rect().height() >= 44.0
                    && button.get_bounding_client_rect().top() - field.get_bounding_client_rect().bottom() >= 16.0,
                    "Bounded pairing retains separated, touch-friendly primary actions")?;
                field.unchecked_ref::<HtmlElement>().blur().unwrap();
                app.remove_attribute("data-native-host").unwrap();
                next_tick().await;
                check(flow.get_bounding_client_rect().width() == browser_width,
                    "Native column styling does not change the browser/PWA layout")?;
                app.set_attribute("data-native-host", "").unwrap();
            }
            app.remove_attribute("style").unwrap();
            state.app_mode.set(AppMode::Workspace);
            state.paired_hosts.set(vec![host.clone(), fixture_host("native-unselected-other", "Other host")]);
            state.connection_statuses.update(|statuses| {
                statuses.insert(host_id.clone(), ConnectionStatus::NeedsAction {
                    code: protocol::MobileAccessErrorCode::ServiceAuthRequired,
                    message: "Sign in again".into(),
                });
            });
            next_tick().await;
            check(state.active_local_host_id.get_untracked().is_none(),
                "The host chooser must exercise recovery before selection")?;
            let unselected_picker = find(".picker-view")?;
            find("[data-mobile-test='paired-host-sign-in']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let recovery = find("[data-mobile-test='native-auth-recovery']")?;
            check(recovery.get_bounding_client_rect().height() > 0.0,
                "Native sign-in failure must remain visible without a selected host")?;
            find("[data-mobile-test='pairing-auth-failed']")?;
            check(state.native_auth_recovery.with_untracked(|pending|
                pending.as_ref().is_some_and(|pending| pending.local_host_id == host_id)),
                "Recovery keeps the exact chooser host without selecting another")?;
            find("[data-mobile-test='native-auth-cancel']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(find(".picker-view")?.is_same_node(Some(&unselected_picker))
                && state.active_local_host_id.get_untracked().is_none()
                && state.native_auth_recovery.get_untracked().is_none()
                && !projected()?.modal,
                "Cancel restores the same unselected chooser and its navigation")?;
            state.connection_statuses.update(|statuses| { statuses.remove(&host_id); });
            state.paired_hosts.set(vec![host.clone()]);
            state.app_mode.set(AppMode::Pairing(crate::state::PairingScreen::Scanner));
            next_tick().await;
            let instruction = find("[data-mobile-test='pairing-scanner-instruction']")?;
            check(instruction.text_content().unwrap().contains("Settings → Mobile")
                && !instruction.text_content().unwrap().contains("phone")
                && !instruction.get_attribute("aria-label").unwrap().contains("phone"),
                "Native scan guidance names the real desktop QR location without assuming an iPhone")?;
            state.app_mode.set(AppMode::Workspace);
            state.active_local_host_id.set(Some(host_id.clone()));
            state.paired_hosts.set(vec![host]);
            state.projects.set(vec![crate::state::ProjectInfo {
                local_host_id: host_id.clone(),
                project: protocol::Project {
                    id: protocol::ProjectId("entry-project".into()),
                    name: "Entry project".into(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone { roots: vec![] },
                },
            }]);
            state.agents.set([("entry-loose", "Loose agent", None),
                ("entry-project-agent", "Project agent", Some(protocol::ProjectId("entry-project".into())))]
                .into_iter().map(|(id, name, project_id)| crate::state::AgentInfo {
                    local_host_id: host_id.clone(),
                    agent_id: protocol::AgentId(id.into()),
                    name: name.into(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: protocol::BackendKind::Claude,
                    workspace_roots: vec![], project_id, parent_agent_id: None,
                    session_id: None, custom_agent_id: None, created_at_ms: 0,
                    instance_stream: protocol::StreamPath(format!("/agent/{id}")),
                    started: true, fatal_error: None, review_recipient: false,
                }).collect());
            next_tick().await;
            for (layout, open) in [
                (NativeIntent::RegularLandscape, true),
                (NativeIntent::RegularLandscape, false),
                (NativeIntent::RegularWidth, false),
                (NativeIntent::CompactWidth, false),
            ] {
                navigate(layout)?;
                next_tick().await;
                for name in ["Loose agent", "Project agent"] {
                    navigate(NativeIntent::Home)?;
                    next_tick().await;
                    find(&format!("[data-mobile-test='home-recent-agent-row'][aria-label='Open chat with {name}']"))?
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    let home_entry = projected()?;
                    check(home_entry.tab == mobile_shell_types::native::NativeTab::Agents
                        && home_entry.route == mobile_shell_types::native::NativeRoute::Chat
                        && home_entry.title == name,
                        "Home agent cards must select the same native Agents conversation destination")?;
                    let hierarchy = find("[data-mobile-test='agents-view']")?;
                    let chat = find("[data-mobile-test='chat-messages']")?;
                    let wide = layout == NativeIntent::RegularLandscape;
                    check(home_entry.sidebar_available == wide && home_entry.sidebar_open == open
                        && (hierarchy.get_bounding_client_rect().height() > 0.0) == open
                        && chat.get_bounding_client_rect().height() > 0.0,
                        "Home entry preserves the layout and deliberate hierarchy visibility choice")?;
                    if open {
                        check(hierarchy.text_content().unwrap().contains("Entry project")
                            && hierarchy.get_bounding_client_rect().right() <= chat.get_bounding_client_rect().left(),
                            "Home entry shows the project/agent hierarchy to the left of the conversation")?;
                    }
                    if !open {
                        navigate(NativeIntent::Back)?;
                        next_tick().await;
                        check(projected()?.tab == mobile_shell_types::native::NativeTab::Agents
                            && hierarchy.get_bounding_client_rect().height() > 0.0,
                            "Back from Home-selected chat returns to Agents without changing compact navigation")?;
                    }
                    find(&format!("[data-mobile-test='agents-view'] [aria-label='Open chat with {name}']"))?
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    check(projected()? == home_entry,
                        "Home and Agents entry must emit identical native presentation")?;
                    if !open && wide {
                        navigate(NativeIntent::ToggleSidebar)?;
                        next_tick().await;
                        check(projected()?.sidebar_open && hierarchy.get_bounding_client_rect().height() > 0.0,
                            "A hidden Home-entry sidebar remains available to Show Agents")?;
                        navigate(NativeIntent::ToggleSidebar)?;
                        next_tick().await;
                    }
                }
                if open {
                    navigate(NativeIntent::ToggleSidebar)?;
                    next_tick().await;
                }
            }
            state.agents.set(vec![]);
            state.projects.set(vec![]);
            state.active_agent.set(None);
            navigate(NativeIntent::RegularLandscape)?;
            navigate(NativeIntent::ToggleSidebar)?;
            navigate(NativeIntent::CompactWidth)?;
            navigate(NativeIntent::Back)?;
            next_tick().await;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            navigate(NativeIntent::NewChat)?;
            next_tick().await;
            check(projected()?.modal, "Project sheet must disable system navigation")?;
            navigate(NativeIntent::Settings)?;
            navigate(NativeIntent::Back)?;
            navigate(NativeIntent::Hosts)?;
            next_tick().await;
            check(projected()?.tab == mobile_shell_types::native::NativeTab::Agents && projected()?.modal,
                "Native tab and Back cannot navigate beneath a modal")?;
            find("[data-mobile-test='project-picker-none']")?
                .dyn_into::<HtmlElement>().map_err(|_| "Project action is not clickable")?.click();
            next_tick().await;
            check(!projected()?.modal, "Closing the sheet restores native navigation")?;
            let composer = find("[data-mobile-test='chat-input']")?
                .dyn_into::<web_sys::HtmlTextAreaElement>().map_err(|_| "Composer is not editable")?;
            state.draft_backend_override.set(Some(protocol::BackendKind::Claude));
            let attach = js_sys::Function::new_with_args("root", r#"
                return new Promise((resolve, reject) => {
                    const observer = new MutationObserver(() => {
                        if (root.querySelector('[data-mobile-test="chat-photo-tray"] img')) {
                            observer.disconnect(); resolve();
                        } else if (root.querySelector('[data-mobile-test="chat-photo-error"]')) {
                            observer.disconnect(); reject(new Error('Photo attachment failed'));
                        }
                    });
                    observer.observe(root, {childList:true, subtree:true, characterData:true});
                    const bytes = Uint8Array.from(atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jXioAAAAASUVORK5CYII='), c => c.charCodeAt(0));
                    const transfer = new DataTransfer();
                    transfer.items.add(new File([bytes], 'draft.png', {type:'image/png'}));
                    const input = root.querySelector('[data-mobile-test="chat-photo-input"]');
                    input.files = transfer.files;
                    input.dispatchEvent(new Event('change', {bubbles:true}));
                });
            "#);
            let attached = attach.call1(&wasm_bindgen::JsValue::NULL, &container).unwrap();
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(attached))
                .await.map_err(|_| "Real DOM photo attachment failed")?;
            composer.set_value("Retained draft");
            composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            composer.set_selection_range(2, 7).unwrap();
            let photo = find("[data-mobile-test='chat-photo-tray'] img")?;
            state.draft_custom_agent_id.set(Some(protocol::CustomAgentId("retained-native-custom".into())));
            state.draft_session_settings.set(protocol::SessionSettingsValues(
                [("model".into(), protocol::SessionSettingValue::String("retained-native-model".into()))].into_iter().collect()));
            navigate(NativeIntent::Home)?;
            next_tick().await;
            navigate(NativeIntent::Hosts)?;
            next_tick().await;
            check(projected()?.route == mobile_shell_types::native::NativeRoute::Hosts,
                "Hosts must be a native navigation detail over the retained workspace")?;
            navigate(NativeIntent::Back)?;
            next_tick().await;
            navigate(NativeIntent::Hosts)?;
            next_tick().await;
            find("[data-mobile-test='paired-host-open']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&composer))
                && composer.value() == "Retained draft"
                && composer.selection_start().unwrap() == Some(2) && composer.selection_end().unwrap() == Some(7)
                && find("[data-mobile-test='chat-photo-tray'] img")?.is_same_node(Some(&photo)),
                "Same-host Hosts/Back and Open trips retain the exact composer, selection and unsent photo")?;
            check(state.draft_backend_override.get_untracked() == Some(protocol::BackendKind::Claude)
                && state.draft_custom_agent_id.get_untracked() == Some(protocol::CustomAgentId("retained-native-custom".into()))
                && !state.draft_session_settings.get_untracked().0.is_empty(),
                "Same-host Open must preserve the draft's backend, custom agent and session settings")?;
            navigate(NativeIntent::Home)?;
            next_tick().await;
            navigate(NativeIntent::Hosts)?;
            next_tick().await;
            find(".picker-pair-another")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            find("[data-mobile-test='pairing-scanner-cancel']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&composer))
                && composer.value() == "Retained draft"
                && composer.selection_start().unwrap() == Some(2) && composer.selection_end().unwrap() == Some(7)
                && find("[data-mobile-test='chat-photo-tray'] img")?.is_same_node(Some(&photo)),
                "Cancelling Pair another host must retain the same host's complete component-owned draft")?;
            navigate(NativeIntent::Sessions)?;
            next_tick().await;
            let search = find("[data-mobile-test='sessions-search']")?
                .dyn_into::<web_sys::HtmlInputElement>().map_err(|_| "Search is not editable")?;
            search.set_value("Retained search");
            search.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            navigate(NativeIntent::Settings)?;
            next_tick().await;
            let shell = find(".mobile-app")?.dyn_into::<HtmlElement>().unwrap();
            let saved_style = shell.style().css_text();
            let saved_statuses = state.connection_statuses.get_untracked();
            for (width, height) in [(320, 520), (386, 700), (820, 540), (1024, 768)] {
                shell.style().set_property("width", &format!("{width}px")).unwrap();
                shell.style().set_property("height", &format!("{height}px")).unwrap();
                navigate(if width >= 820 && width > height { NativeIntent::RegularLandscape }
                    else if width >= 820 { NativeIntent::RegularWidth } else { NativeIntent::CompactWidth })?;
                for status in [ConnectionStatus::Connected, ConnectionStatus::Disconnected] {
                    state.connection_statuses.update(|statuses| { statuses.insert(host_id.clone(), status); });
                    for intent in [NativeIntent::Home, NativeIntent::Agents, NativeIntent::Sessions, NativeIntent::Settings] {
                        navigate(intent)?;
                        next_tick().await;
                        let hud = find("[data-mobile-test='connection-banner']")?.get_bounding_client_rect();
                        let content = find(".mobile-content")?;
                        check(hud.height() > 0.0 && content.get_bounding_client_rect().top() >= hud.bottom(),
                            "Native connection feedback must reserve layout space above every screen")?;
                        let controls = content.query_selector_all("button,input,textarea,select,a,[role='button'],[role='option']").unwrap();
                        for index in 0..controls.length() {
                            let control = controls.item(index).unwrap().dyn_into::<web_sys::Element>().unwrap();
                            let bounds = control.get_bounding_client_rect();
                            if bounds.width() > 0.0 && bounds.height() > 0.0 {
                                check(bounds.right() <= hud.left() || bounds.left() >= hud.right()
                                    || bounds.bottom() <= hud.top() || bounds.top() >= hud.bottom(),
                                    "Native connection feedback must not cover an interactive control")?;
                            }
                        }
                    }
                }
            }
            shell.style().set_css_text(&saved_style);
            state.connection_statuses.set(saved_statuses);
            navigate(NativeIntent::CompactWidth)?;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&composer)),
                "Native tab switches must not remount the composer")?;
            check(composer.value() == "Retained draft", "Native tab switch preserves draft text")?;
            find("[data-mobile-test='chat-photo-tray'] img")?;
            check(composer.selection_start().unwrap() == Some(2) && composer.selection_end().unwrap() == Some(7),
                "Native tab switch preserves editor selection")?;
            crate::actions::native_sign_in_and_reconnect(state.clone(), host_id.clone(), bridge::AuthProvider::Apple);
            next_tick().await;
            find("[data-mobile-test='native-auth-recovery']")?;
            find("[data-mobile-test='native-auth-cancel']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&composer)) && composer.value() == "Retained draft",
                "A failed native sign-in must not dispose the retained composer")?;
            check(composer.selection_start().unwrap() == Some(2) && composer.selection_end().unwrap() == Some(7),
                "Cancelling auth recovery preserves editor selection")?;
            navigate(NativeIntent::RegularLandscape)?;
            next_tick().await;
            let agents = find("[data-mobile-test='agents-view']")?;
            let width = agents.get_bounding_client_rect().width();
            let pane = find(".native-agents-pane")?;
            let workspace = find(".native-workspace")?;
            check(width >= 280.0, &format!(
                "Wide hierarchy remains touch usable: content={width}, pane={}, workspace={}, hidden={}, columns={}",
                pane.get_bounding_client_rect().width(), workspace.get_bounding_client_rect().width(),
                pane.has_attribute("hidden"), window.get_computed_style(&workspace).unwrap().unwrap()
                    .get_property_value("grid-template-columns").unwrap()
            ))?;
            let retained_agent = state.active_agent.get_untracked();
            let filter = find("[data-mobile-test='agents-search']")?
                .dyn_into::<web_sys::HtmlInputElement>().map_err(|_| "Agent filter is not editable")?;
            filter.set_value("Retained filter");
            filter.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            let transcript = find("[data-mobile-test='chat-messages']")?;
            for (intent, expected_open, expected_available) in [
                (NativeIntent::ToggleSidebar, false, true),
                (NativeIntent::RegularWidth, false, false),
                (NativeIntent::CompactWidth, false, false),
                (NativeIntent::RegularLandscape, false, true),
                (NativeIntent::ToggleSidebar, true, true),
                (NativeIntent::RegularWidth, false, false),
                (NativeIntent::RegularLandscape, true, true),
                (NativeIntent::CompactWidth, false, false),
                (NativeIntent::RegularLandscape, true, true),
            ] {
                navigate(intent)?;
                next_tick().await;
                let presentation = projected()?;
                let visible = agents.get_bounding_client_rect().height() > 0.0;
                check(visible == expected_open && presentation.sidebar_open == expected_open,
                    "Toggle closes/reopens the hierarchy and layout changes preserve the user's choice")?;
                check(presentation.sidebar_available == expected_available,
                    "Only expanded landscape offers the pinned hierarchy")?;
                check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&composer))
                    && composer.value() == "Retained draft"
                    && state.active_agent.get_untracked() == retained_agent
                    && find("[data-mobile-test='chat-messages']")?.is_same_node(Some(&transcript))
                    && find("[data-mobile-test='agents-search']")?.is_same_node(Some(&filter))
                    && filter.value() == "Retained filter",
                    "Layout and sidebar transitions retain composer, transcript, selection and filter DOM")?;
            }
            let swarm = crate::components::fixture_swarm();
            state.swarms_by_host.update(|hosts| {
                hosts.insert(host_id.clone(), std::collections::HashMap::from([(swarm.id.clone(), swarm)]));
            });
            next_tick().await;
            find("[data-mobile-test='agents-segment-swarms']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(filter.get_bounding_client_rect().height() == 0.0,
                "The Agents filter is not offered for the unrelated Swarms list")?;
            find("[data-mobile-test='agents-segment-agents']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(find("[data-mobile-test='agents-search']")?.is_same_node(Some(&filter))
                && filter.value() == "Retained filter" && filter.get_bounding_client_rect().height() >= 44.0,
                "Returning to Agents restores its retained, touch-usable filter")?;
            filter.set_value("");
            filter.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            state.swarms_by_host.update(|hosts| { hosts.remove(&host_id); });
            state.session_settings_open.set(true);
            next_tick().await;
            navigate(NativeIntent::ToggleSidebar)?;
            next_tick().await;
            check(projected()?.modal && projected()?.sidebar_open,
                "A modal blocks native sidebar navigation without losing its open state")?;
            state.session_settings_open.set(false);
            navigate(NativeIntent::CompactWidth)?;
            next_tick().await;
            check(agents.get_bounding_client_rect().height() == 0.0,
                "Compact chat has no competing global sidebar")?;
            navigate(NativeIntent::Sessions)?;
            next_tick().await;
            check(find("[data-mobile-test='sessions-search']")?.is_same_node(Some(&search)) && search.value() == "Retained search",
                "Native tabs retain the session filter control and its text")?;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            navigate(NativeIntent::Back)?;
            next_tick().await;
            check(agents.get_bounding_client_rect().height() > 0.0, "Back reveals the retained Agents hierarchy")?;
            navigate(NativeIntent::NewChat)?;
            next_tick().await;
            find("[data-mobile-test='project-picker-none']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let fresh = find("[data-mobile-test='chat-input']")?
                .dyn_into::<web_sys::HtmlTextAreaElement>().unwrap();
            check(!fresh.is_same_node(Some(&composer)) && fresh.value().is_empty(),
                "An explicit new conversation owns a fresh complete composer")?;
            check(container.query_selector("[data-mobile-test='chat-photo-tray']").unwrap().is_none(),
                "A previous draft's photo must never transfer to a new conversation")?;

            state.draft_backend_override.set(Some(protocol::BackendKind::Claude));
            let hold_photo = js_sys::Function::new_with_args("root", r#"
                return new Promise(started => {
                    const original = File.prototype.arrayBuffer;
                    File.prototype.arrayBuffer = function() {
                        File.prototype.arrayBuffer = original;
                        return original.call(this).then(buffer => new Promise(finish => {
                            started(() => finish(buffer));
                        }));
                    };
                    const transfer = new DataTransfer();
                    transfer.items.add(new File(['photo'], 'pending.png', {type:'image/png'}));
                    const input = root.querySelector('[data-mobile-test="chat-photo-input"]');
                    input.files = transfer.files;
                    input.dispatchEvent(new Event('change', {bubbles:true}));
                });
            "#);
            let reading = hold_photo.call1(&wasm_bindgen::JsValue::NULL, &container).unwrap();
            let finish_read = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(reading))
                .await.map_err(|_| "Could not observe the pending photo read")?
                .dyn_into::<js_sys::Function>().map_err(|_| "Missing photo completion event")?;
            navigate(NativeIntent::Back)?;
            navigate(NativeIntent::NewChat)?;
            next_tick().await;
            find("[data-mobile-test='project-picker-none']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let after_pending_photo = find("[data-mobile-test='chat-input']")?;
            check(!after_pending_photo.is_same_node(Some(&fresh)), "A pending file read cannot retain the discarded composer")?;
            finish_read.call0(&wasm_bindgen::JsValue::NULL).unwrap();
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&after_pending_photo)) &&
                container.query_selector("[data-mobile-test='chat-photo-tray']").unwrap().is_none(),
                "Completing a discarded draft's read must not mutate the next composer")?;

            let host_stream = make_host_stream();
            state.host_streams.update(|streams| { streams.insert(host_id.clone(), host_stream.clone()); });
            reset_inbound_seq_for_host(&host_id);
            let mut settings = empty_host_settings();
            settings.default_backend = Some(protocol::BackendKind::Claude);
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::HostSettings, 0,
                    &protocol::HostSettingsPayload { settings, etag: Default::default(), configured_secrets: Vec::new() }).unwrap());
            next_tick().await;
            let new_composer = find("[data-mobile-test='chat-input']")?.dyn_into::<web_sys::HtmlTextAreaElement>().unwrap();
            new_composer.set_value("A new conversation request");
            new_composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            // Flush the input-driven disabled binding before requesting the real button action.
            next_tick().await;
            let send = find("[data-mobile-test='chat-send']")?;
            check(!send.has_attribute("disabled"), "The entered new-chat draft must enable its actual Send button")?;
            send.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let creation = state.native_chat_destination.get_untracked().ok_or("New chat must own its explicit creation stream")?;
            check(creation.stream.spawn_parent().as_ref() == Some(&host_stream), "New chat must address its own connection's creation child")?;
            let current = fixture_user_agent("current-chat", "current-session");
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::NewAgent, 1, &current).unwrap());
            next_tick().await;
            check(state.active_agent.get_untracked().is_none(), "Discovery alone is not a new-chat completion")?;
            let published: protocol::AgentStartPayload = serde_json::from_value(serde_json::to_value(&current).unwrap()).unwrap();
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(creation.stream, FrameKind::AgentStart, 0, &published).unwrap());
            next_tick().await;
            let current_composer = find("[data-mobile-test='chat-input']")?
                .dyn_into::<web_sys::HtmlTextAreaElement>().map_err(|_| "Current composer is not editable")?;
            current_composer.set_value("Current conversation draft");
            current_composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            current_composer.set_selection_range(2, 7).unwrap();
            navigate(NativeIntent::Settings)?;
            next_tick().await;
            let background = fixture_user_agent("same-host-background-chat", "background-session");
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::NewAgent, 2, &background).unwrap());
            next_tick().await;
            check(projected()?.tab == mobile_shell_types::native::NativeTab::Settings,
                "An unrelated same-host user-agent announcement must not navigate the current tab")?;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&current_composer))
                && current_composer.value() == "Current conversation draft"
                && current_composer.selection_start().unwrap() == Some(2)
                && current_composer.selection_end().unwrap() == Some(7)
                && state.active_agent.get_untracked().as_ref().map(|active| &active.agent_id) == Some(&current.agent_id),
                "An unrelated same-host announcement must retain the exact conversation, draft control and editor selection")?;
            let agent_ref = crate::state::AgentRef { local_host_id: host_id.clone(), agent_id: current.agent_id.clone() };
            state.agent_loaded.update(|loaded| { loaded.insert(agent_ref.clone()); });
            for index in 0..30 {
                state.push_chat_message_entry(&agent_ref, crate::state::ChatMessageEntry {
                    message: protocol::ChatMessage {
                        origin: None, message_id: None, timestamp: index,
                        sender: protocol::MessageSender::User,
                        content: format!("Retained transcript row {index}\nA second line\nA third line"),
                        reasoning: None, tool_calls: Vec::new(), model_info: None,
                        token_usage: None, context_breakdown: None, images: None,
                    }, tool_requests: Vec::new(),
                });
            }
            next_tick().await;
            let scroller = find("[data-mobile-test='chat-messages']")?.dyn_into::<HtmlElement>().unwrap();
            scroller.set_scroll_top(200);
            scroller.dispatch_event(&web_sys::Event::new("scroll").unwrap()).unwrap();
            check(scroller.scroll_top() == 200, "The rendered transcript is genuinely scrollable")?;
            let saved_layout = shell.style().css_text();
            for (width, height, layout, sidebar) in [(951, 669, NativeIntent::RegularLandscape, true),
                (669, 951, NativeIntent::RegularWidth, false), (466, 678, NativeIntent::CompactWidth, false),
                (951, 669, NativeIntent::RegularLandscape, true)] {
                shell.style().set_property("width", &format!("{width}px")).unwrap();
                shell.style().set_property("height", &format!("{height}px")).unwrap();
                navigate(layout)?;
                next_tick().await;
                check(scroller.scroll_top() == 200
                    && find("[data-mobile-test='chat-input']")?.is_same_node(Some(&current_composer))
                    && current_composer.value() == "Current conversation draft"
                    && current_composer.selection_start().unwrap() == Some(2)
                    && current_composer.selection_end().unwrap() == Some(7)
                    && state.active_agent.get_untracked().as_ref().map(|agent| &agent.agent_id) == Some(&current.agent_id),
                    "DOM size changes retain the same selected conversation, scroll offset and draft selection")?;
                check((agents.get_bounding_client_rect().height() > 0.0) == sidebar,
                    "The DOM hierarchy is pinned only in expanded landscape")?;
                let send = find("[data-mobile-test='chat-send']")?.get_bounding_client_rect();
                let bounds = shell.get_bounding_client_rect();
                check(send.left() >= bounds.left() && send.right() <= bounds.right()
                    && send.top() >= bounds.top() && send.bottom() <= bounds.bottom(),
                    "Send remains inside the resized DOM viewport")?;
            }
            shell.style().set_css_text(&saved_layout);
            navigate(NativeIntent::CompactWidth)?;
            next_tick().await;
            find("[data-mobile-test='chat-send-menu-toggle']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            find("[data-mobile-test='chat-send-menu-ask-aside']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let fork = state.native_chat_destination.get_untracked().ok_or("Fork + send must own a creation stream even with another chat open")?;
            check(fork.source_agent.as_ref().map(|active| &active.agent_id) == Some(&current.agent_id), "Fork captures its actual source conversation")?;
            // Fork settlement is NewChat-scoped: it retains the source composer until publication.
            check(current_composer.value() == "Current conversation draft"
                && state.active_agent.get_untracked().as_ref().map(|active| &active.agent_id) == Some(&current.agent_id),
                "Pending Fork + send must retain its actual source conversation and complete draft")?;
            let fork_frames = bridge::test_sent_lines().into_iter()
                .filter_map(|line| serde_json::from_str::<protocol::Envelope>(&line).ok())
                .filter(|frame| frame.kind == FrameKind::SpawnAgent && frame.stream == fork.stream)
                .collect::<Vec<_>>();
            check(fork_frames.len() == 1 && matches!(fork_frames[0].parse_payload::<protocol::SpawnAgentPayload>().unwrap().params,
                protocol::SpawnAgentParams::Fork { from_session_id, prompt, .. }
                    if Some(&from_session_id) == current.session_id.as_ref() && prompt == "Current conversation draft"),
                "Fork + send must admit exactly one operation with the actual source session and complete text")?;
            current_composer.set_value("Draft written after the fork submission");
            current_composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
            current_composer.set_selection_range(2, 7).unwrap();
            let added = attach.call1(&wasm_bindgen::JsValue::NULL, &container).unwrap();
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(added))
                .await.map_err(|_| "Could not attach the source draft's new photo")?;
            let edited_photo = find("[data-mobile-test='chat-photo-tray'] img")?;
            let forked = fixture_user_agent("published-fork", "published-fork-session");
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(fork.stream.clone(), FrameKind::AgentStart, 0,
                    &serde_json::from_value::<protocol::AgentStartPayload>(serde_json::to_value(&forked).unwrap()).unwrap()).unwrap());
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::NewAgent, 3, &forked).unwrap());
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&current_composer))
                && current_composer.value() == "Draft written after the fork submission"
                && current_composer.selection_start().unwrap() == Some(2)
                && current_composer.selection_end().unwrap() == Some(7)
                && find("[data-mobile-test='chat-photo-tray'] img")?.is_same_node(Some(&edited_photo))
                && state.active_agent.get_untracked().as_ref().map(|active| &active.agent_id) == Some(&current.agent_id),
                "The same non-superseded Fork completion must not discard later source text, selection or photos")?;
            // A separate explicit creation is then superseded by Resume.
            find("[data-mobile-test='chat-send-menu-toggle']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            find("[data-mobile-test='chat-send-menu-ask-aside']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let photo_fork = state.native_chat_destination.get_untracked().ok_or("A second explicit Fork owns its own operation")?;
            find(".chat-photo-remove")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let photo_forked = fixture_user_agent("published-photo-fork", "published-photo-fork-session");
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(photo_fork.stream, FrameKind::AgentStart, 0,
                    &serde_json::from_value::<protocol::AgentStartPayload>(serde_json::to_value(&photo_forked).unwrap()).unwrap()).unwrap());
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::NewAgent, 4, &photo_forked).unwrap());
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&current_composer))
                && current_composer.value() == "Draft written after the fork submission"
                && container.query_selector("[data-mobile-test='chat-photo-tray']").unwrap().is_none()
                && state.active_agent.get_untracked().as_ref().map(|active| &active.agent_id) == Some(&current.agent_id),
                "A photo-only source edit must also retire automatic Fork navigation without changing its text or conversation")?;
            find("[data-mobile-test='chat-send-menu-toggle']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            find("[data-mobile-test='chat-send-menu-ask-aside']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let superseded_fork = state.native_chat_destination.get_untracked().ok_or("The later Fork owns a distinct operation before Resume")?;
            state.sessions.set(vec![crate::state::SessionInfo {
                local_host_id: host_id.clone(), summary: protocol::SessionSummary {
                    id: protocol::SessionId("requested-session".into()), backend_kind: protocol::BackendKind::Claude,
                    launch_profile_id: None, workspace_roots: Vec::new(), project_id: None,
                    alias: Some("Retained search".into()), user_alias: None, parent_id: None,
                    created_at_ms: 0, updated_at_ms: 0, message_count: 0, token_count: None, resumable: true,
                    compacted_from_session_id: None, compacted_to_session_id: None,
                    compacted_at_ms: None, compaction_summary_preview: None,
                },
            }]);
            navigate(NativeIntent::Sessions)?;
            next_tick().await;
            find("[data-mobile-test='session-resume']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(bridge::test_sent_lines().iter().any(|line| {
                serde_json::from_str::<protocol::Envelope>(line).is_ok_and(|frame| frame.kind == FrameKind::SpawnAgent)
            }), "The resume click must cross the production protocol-send seam")?;
            check(projected()?.route == mobile_shell_types::native::NativeRoute::Root,
                "Pending session resume must not relabel the existing chat as its result")?;
            navigate(NativeIntent::Agents)?;
            next_tick().await;
            check(find("[data-mobile-test='chat-input']")?.is_same_node(Some(&current_composer)) &&
                current_composer.value() == "Draft written after the fork submission",
                "A pending session resume retains the current chat and its draft")?;
            let failed_creation = state.native_chat_destination.get_untracked().ok_or("Resume must retain its operation")?;
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(failed_creation.stream.clone(), FrameKind::CommandError, 0,
                    &protocol::CommandErrorPayload {
                        workbench_reason: None, context: None, request_id: None, stream: failed_creation.stream.clone(),
                        request_kind: FrameKind::SpawnAgent, operation: "resume".into(),
                        code: protocol::CommandErrorCode::Internal, message: "Session could not resume".into(), fatal: false,
                    }).unwrap());
            next_tick().await;
            check(projected()?.tab == mobile_shell_types::native::NativeTab::Agents &&
                current_composer.value() == "Draft written after the fork submission",
                "Failed resume leaves the original conversation intact")?;
            navigate(NativeIntent::Sessions)?;
            next_tick().await;
            find("[data-mobile-test='session-resume']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let resumed = fixture_user_agent("resumed-chat", "requested-session");
            let resume_creation = state.native_chat_destination.get_untracked().ok_or("The second Resume owns a distinct stream")?;
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(superseded_fork.stream, FrameKind::AgentStart, 0,
                    &serde_json::from_value::<protocol::AgentStartPayload>(serde_json::to_value(&background).unwrap()).unwrap()).unwrap());
            check(state.native_chat_destination.get_untracked().as_ref().map(|target| &target.stream) == Some(&resume_creation.stream), "A superseded fork's publication cannot consume the later Resume destination")?;
            check(resume_creation.stream != failed_creation.stream, "Distinct requests cannot share a terminal stream")?;
            let published: protocol::AgentStartPayload = serde_json::from_value(serde_json::to_value(&resumed).unwrap()).unwrap();
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(resume_creation.stream, FrameKind::AgentStart, 0, &published).unwrap());
            next_tick().await;
            check(state.active_agent.get_untracked().as_ref().map(|active| &active.agent_id) == Some(&current.agent_id), "A publication receipt must wait for this subscriber's typed descriptor")?;
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream.clone(), FrameKind::NewAgent, 5, &resumed).unwrap());
            next_tick().await;
            let resumed_projection = projected()?;
            check(resumed_projection.tab == mobile_shell_types::native::NativeTab::Sessions &&
                resumed_projection.route == mobile_shell_types::native::NativeRoute::Chat && resumed_projection.title == "resumed-chat",
                &format!("Only the matching server NewAgent commits the Sessions chat destination: tab={:?}, route={:?}, title_matches={}, pending={}",
                    resumed_projection.tab, resumed_projection.route, resumed_projection.title == "resumed-chat",
                    state.native_chat_destination.get_untracked().is_some()))?;
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(resumed.instance_stream.clone(), FrameKind::AgentActivityStats, 0,
                    &protocol::AgentActivityStatsPayload {
                        agent_id: resumed.agent_id.clone(), stats: protocol::AgentActivityStats {
                            current_context_usage: Some(protocol::CurrentContextUsage::Known { input_tokens: 98, context_window: 100 }),
                            ..Default::default()
                        },
                    }).unwrap());
            next_tick().await;
            let context = find("[data-mobile-test='chat-context-bar']")?;
            check(context.get_attribute("aria-valuenow").as_deref() == Some("98") &&
                context.get_bounding_client_rect().height() > 0.0,
                "Native content retains visible accessible server context occupancy")?;
            dispatch_envelope(&state, &host_id,
                protocol::Envelope::from_payload(host_stream, FrameKind::Welcome, 6,
                    &protocol::WelcomePayload {
 mobile_client_bootstrap: None,
                        protocol_version: protocol::PROTOCOL_VERSION, tyde_version: protocol::TYDE_VERSION,
                        release_version: Some(protocol::TydeReleaseVersion::parse("0.9.4-beta.12").unwrap()),
                    }).unwrap());
            next_tick().await;
            // Actual30's approved single native error replaces the duplicate PWA loader paragraph.
            let mismatch_visible = || container.query_selector("[data-mobile-test='mobile-shell-error-banner'][data-error-code='VersionMismatch']").unwrap()
                .is_some_and(|banner| banner.get_bounding_client_rect().height() > 0.0
                    && banner.text_content().unwrap().contains("Update Tyde host to support native client bootstrap."))
                && matches!(state.active_host_connection_status(), ConnectionStatus::UpdateRequired { .. });
            check(mismatch_visible(), "A host without the bootstrap baseline must visibly require a host update")?;
            apply_disconnect(&state, &host_id, None);
            next_tick().await;
            check(mismatch_visible(), "Disconnect must not erase the native compatibility verdict")?;
            // Actual27 iPad frames showed a fixed shell alert covering the pairing title/Cancel.
            let mut error_failures = Vec::new();
            let failure = state.mobile_shell_error.get_untracked().ok_or("Missing native release failure")?;
            if failure.code != protocol::MobileAccessErrorCode::VersionMismatch {
                error_failures.push("A rejected native release must retain the typed VersionMismatch reason".to_owned());
            }
            state.app_mode.set(AppMode::Pairing(crate::state::PairingScreen::ManualPaste));
            for (width, height) in [(320, 700), (390, 390), (834, 700), (1210, 390)] {
                // The runner report places this mounted root below its 437px viewport.
                // Anchor the tested app on-screen before asserting actual pointer delivery.
                app.set_attribute("style", &format!("position:fixed;top:0;left:0;width:{width}px;height:{height}px")).unwrap();
                next_tick().await;
                let banner = find("[data-mobile-test='mobile-shell-error-banner']")?;
                let text = banner.text_content().unwrap_or_default();
                if text.contains("TransportFailed") || text.contains("VersionMismatch") || text.contains("protocol validation failed") {
                    error_failures.push("Native primary error copy must not expose a debug code or transport prefix".into());
                }
                if !text.contains("Update Tyde host to support native client bootstrap") {
                    error_failures.push("A native release failure must keep its actionable update reason".into());
                }
                let header = find(".pairing-view .view-header")?.get_bounding_client_rect();
                let alert = banner.get_bounding_client_rect();
                if alert.bottom() > header.top() {
                    error_failures.push(format!("Native error covers pairing navigation at {width}x{height}: alert bottom={}, header top={}", alert.bottom(), header.top()));
                }
                let cancel = find(".pairing-view .header-action")?;
                let bounds = cancel.get_bounding_client_rect();
                if width == 320 {
                    let hit = window.document().unwrap().element_from_point(
                        (bounds.left()+bounds.width()/2.0) as f32,
                        (bounds.top()+bounds.height()/2.0) as f32,
                    );
                    if !hit.is_some_and(|hit| cancel.contains(Some(&hit))) {
                        let geometry = js_sys::Function::new_with_args("cancel", r#"
                            const r = cancel.getBoundingClientRect();
                            const at = document.elementFromPoint(r.x+r.width/2, r.y+r.height/2);
                            const describe = el => el && ({tag:el.tagName, class:el.className,
                                rect:el.getBoundingClientRect().toJSON(), display:getComputedStyle(el).display,
                                visibility:getComputedStyle(el).visibility, pointer:getComputedStyle(el).pointerEvents});
                            return JSON.stringify({cancel:describe(cancel), hit:describe(at),
                                header:describe(cancel.closest('.view-header')), app:describe(cancel.closest('.mobile-app')),
                                viewport:[innerWidth,innerHeight,scrollX,scrollY]});
                        "#).call1(&wasm_bindgen::JsValue::NULL, &cancel).unwrap().as_string().unwrap();
                        error_failures.push(format!("Native Cancel must receive pointer input while an error is visible: {geometry}"));
                    }
                }
                let close = find("[data-mobile-test='mobile-shell-error-dismiss']")?.get_bounding_client_rect();
                if close.width() < 44.0 || close.height() < 44.0 {
                    error_failures.push("Native error dismissal needs a usable touch target".into());
                }
                let message = find(".mobile-shell-error-message")?;
                if window.get_computed_style(&message).unwrap().unwrap().get_property_value("white-space").unwrap() == "nowrap" {
                    error_failures.push("Native recovery instructions must wrap instead of clipping".into());
                }
            }
            find(".pairing-view .header-action")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(state.app_mode.get_untracked() == AppMode::Workspace,
                "A native error must not trap the user in pairing")?;
            find("[data-mobile-test='mobile-shell-error-dismiss']")?.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(state.mobile_shell_error.get_untracked().is_none()
                && matches!(state.host_releases.get_untracked().get(&host_id), Some(crate::bundle::HostRelease::NativeIncompatible(_))),
                "Dismissing presentation must not clear or bypass the typed compatibility verdict")?;
            check(error_failures.is_empty(), &error_failures.join("; "))?;
            Ok(())
        }.await;
        drop(handle);
        container.remove();
        PAGE_HIDDEN_SINCE_MS.with(|hidden| hidden.set(None));
        js_sys::Reflect::delete_property(&window, &"__tydeNativeHost".into()).unwrap();
        restore_loader_boundary();
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let browser = make_container();
        let browser_state = state.clone();
        let browser_handle = mount_to(browser.clone(), move || {
            provide_context(browser_state);
            view! { <AppSurface /> }
        });
        next_tick().await;
        // Browser chat deliberately has Back instead of global tabs. The native
        // flow ended in chat, so exercise the existing browser navigation first.
        assert!(
            browser
                .query_selector("[data-mobile-test='bottom-nav']")
                .unwrap()
                .is_none()
        );
        browser
            .query_selector("[data-mobile-test='chat-back']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            browser
                .query_selector("[data-mobile-test='bottom-nav']")
                .unwrap()
                .is_some()
        );
        drop(browser_handle);
        browser.remove();
    }

    #[wasm_bindgen_test]
    async fn compatible_welcome_defers_exact_release_switch_without_losing_draft() {
        install_loader_boundary();
        let _sends = bridge::test_clean_sends();
        let state = AppState::new();
        let host = LocalHostId("bundle-selected".into());
        let stream = make_host_stream();
        state.app_mode.set(AppMode::Workspace);
        state.active_local_host_id.set(Some(host.clone()));
        state.host_streams.update(|m| {
            m.insert(host.clone(), stream.clone());
        });
        state.chat_input.set("unsent draft".into());
        let container = make_container();
        let mounted_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted_state);
            view! { <AppSurface /> }
        });
        dispatch_envelope(
            &state,
            &host,
            protocol::Envelope::from_payload(
                stream,
                FrameKind::Welcome,
                0,
                &protocol::WelcomePayload {
                    mobile_client_bootstrap: None,
                    protocol_version: protocol::PROTOCOL_VERSION,
                    tyde_version: protocol::TYDE_VERSION,
                    release_version: Some(
                        protocol::TydeReleaseVersion::parse("0.9.4-beta.12").unwrap(),
                    ),
                },
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Finish or save"),
            "a compatible Welcome must visibly defer exact-host switching while a draft exists"
        );
        assert!(
            state.chat_input.get_untracked() == "unsent draft",
            "draft is retained"
        );
        assert_eq!(loader_preparations(), 0);

        let background = LocalHostId("bundle-background".into());
        let background_stream = make_host_stream();
        state.host_streams.update(|m| {
            m.insert(background.clone(), background_stream.clone());
        });
        dispatch_envelope(
            &state,
            &background,
            protocol::Envelope::from_payload(
                background_stream,
                FrameKind::Reject,
                0,
                &protocol::RejectPayload {
                    code: protocol::RejectCode::IncompatibleProtocol,
                    message: "test mismatch".into(),
                    server_protocol_version: protocol::PROTOCOL_VERSION + 1,
                    server_tyde_version: protocol::TYDE_VERSION,
                    release_version: Some(
                        protocol::TydeReleaseVersion::parse("0.9.4-beta.13").unwrap(),
                    ),
                },
            )
            .unwrap(),
        );
        next_tick().await;
        assert_eq!(
            loader_preparations(),
            0,
            "background rejection must never switch"
        );
        state.active_local_host_id.set(Some(background.clone()));
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Finish or save")
        );
        assert!(
            loader_owns_target(&background.0, "0.9.4-beta.13"),
            "deferred selected host must own the next cold boot"
        );
        assert_eq!(loader_preparations(), 0);
        state.active_local_host_id.set(None);
        next_tick().await;
        assert!(
            loader_no_target(),
            "picker must not retain a deselected target"
        );
        state.active_local_host_id.set(Some(host.clone()));
        next_tick().await;
        assert!(loader_owns_target(&host.0, "0.9.4-beta.12"));
        state.viewing_chat.set(true);
        state.chat_input.set(String::new());
        next_tick().await;
        assert_eq!(
            loader_preparations(),
            0,
            "mounted composer may own attachments or queued edits"
        );
        state.viewing_chat.set(false);
        state.session_settings_open.set(true);
        next_tick().await;
        assert_eq!(loader_preparations(), 0, "settings editor must defer");
        state.session_settings_open.set(false);
        state.voice_ui.set(crate::voice::MobileVoiceState::Failed(
            "voice recovery".into(),
        ));
        next_tick().await;
        assert_eq!(loader_preparations(), 0, "voice recovery must defer");
        state.hold_submission(crate::state::PendingSubmission {
            human_input_id: protocol::HumanInputId(uuid::Uuid::new_v4().to_string()),
            failure_reason: None,
            action: protocol::HumanInputAction::Send,
            local_submission_id: bridge::LocalSubmissionId(9999),
            origin: state.mint_submission_origin(),
            local_host_id: host.clone(),
            connection_instance_id: 17,
            target: crate::state::SubmissionTarget::NewChat,
            text: "pending fixture".into(),
            images: Vec::new(),
            tool_response: None,
            state: crate::state::PendingSubmissionState::QueuedLocally,
        });
        state.voice_ui.set(crate::voice::MobileVoiceState::Idle);
        next_tick().await;
        assert_eq!(loader_preparations(), 0, "pending submissions must defer");
        state.apply_submission_outcome(
            bridge::LocalSubmissionId(9999),
            17,
            bridge::SubmissionTransportOutcome::TransportAcknowledged,
        );
        state.boot_handoff_complete.set(false);
        next_tick().await;
        assert_eq!(
            loader_preparations(),
            0,
            "boot auth must finish before switching"
        );
        state.boot_handoff_complete.set(true);
        next_tick().await;
        assert_eq!(loader_preparations(), 1);
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Preparing exact host release")
        );
        state.active_local_host_id.set(Some(background.clone()));
        next_tick().await;
        assert_eq!(
            loader_preparations(),
            2,
            "selection must choose the background rejection now"
        );
        finish_loader_prepare(0, "");
        next_tick().await;
        assert_eq!(loader_reloads(), 0, "superseded preparation cannot reload");
        finish_loader_prepare(1, "unpublished");
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("has not been published")
        );
        assert_eq!(loader_reloads(), 0);
        state.active_local_host_id.set(Some(host.clone()));
        next_tick().await;
        assert_eq!(loader_preparations(), 3, "deliberate A > B > A is allowed");
        // A new draft can race the promise completion before effects flush.
        state.chat_input.set("new unsent draft".into());
        finish_loader_prepare(2, "");
        next_tick().await;
        assert_eq!(loader_reloads(), 0);
        assert!(
            state.chat_input.get_untracked() == "new unsent draft",
            "new draft is retained"
        );
        state.chat_input.set(String::new());
        next_tick().await;
        assert_eq!(loader_preparations(), 4);
        apply_disconnect(&state, &host, None);
        finish_loader_prepare(3, "");
        next_tick().await;
        assert_eq!(loader_reloads(), 0, "disconnected Welcome loses authority");
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Preparing exact host release")
        );

        let stream = make_host_stream();
        state.host_streams.update(|m| {
            m.insert(host.clone(), stream.clone());
        });
        dispatch_envelope(
            &state,
            &host,
            protocol::Envelope::from_payload(
                stream.clone(),
                FrameKind::Welcome,
                0,
                &protocol::WelcomePayload {
                    mobile_client_bootstrap: None,
                    protocol_version: protocol::PROTOCOL_VERSION,
                    tyde_version: protocol::TYDE_VERSION,
                    release_version: Some(
                        protocol::TydeReleaseVersion::parse("0.9.4-beta.11").unwrap(),
                    ),
                },
            )
            .unwrap(),
        );
        next_tick().await;
        assert_eq!(
            loader_preparations(),
            4,
            "same exact executing release does not reload"
        );
        reset_inbound_seq_for_host(&host);
        dispatch_envelope(
            &state,
            &host,
            protocol::Envelope::from_payload(
                stream,
                FrameKind::Welcome,
                0,
                &protocol::WelcomePayload {
                    mobile_client_bootstrap: None,
                    protocol_version: protocol::PROTOCOL_VERSION,
                    tyde_version: protocol::TYDE_VERSION,
                    release_version: None,
                },
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                // MissingRelease now names the client explicitly; absence must remain visible.
                .contains("did not provide a client release")
        );
        assert_eq!(loader_preparations(), 4);
        state.active_local_host_id.set(Some(background));
        next_tick().await;
        assert_eq!(loader_preparations(), 5);
        finish_loader_prepare(4, "");
        next_tick().await;
        assert_eq!(
            loader_reloads(),
            1,
            "safe selected-host switch commits once"
        );
        js_sys::Reflect::set(
            &web_sys::window().unwrap(),
            &"__tydeLoader".into(),
            &wasm_bindgen::JsValue::NULL,
        )
        .unwrap();
        state.active_tab.set(MobileTab::Settings);
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("no compatible release loader")
        );
        assert_eq!(loader_reloads(), 1, "missing bridge never reloads");
        drop(handle);
        restore_loader_boundary();
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn bundle_selection_survives_replay_and_multiple_hosts_require_a_choice() {
        install_loader_boundary();
        let _sends = bridge::test_clean_sends();
        let state = AppState::new();
        state.app_mode.set(AppMode::Workspace);
        let a = fixture_host("bundle-choice-a", "First host");
        let b = fixture_host("bundle-choice-b", "Second host");
        let c = fixture_host("bundle-choice-c", "Third host");
        crate::bridge::loader::save_selection(Some(&b.local_host_id)).unwrap();
        crate::bundle::restore_selection(&state);
        apply_paired_hosts_list(&state, vec![a.clone(), b.clone(), c.clone()]);
        let container = make_container();
        let mounted = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted);
            view! { <AppSurface /> }
        });
        apply_connection_status(
            &state,
            a.local_host_id.clone(),
            PairedHostConnectionStatus::Connected,
            Some(101),
        );
        next_tick().await;
        assert!(container.text_content().unwrap().contains("Second host"));
        assert_eq!(
            state.active_local_host_id.get_untracked(),
            Some(b.local_host_id.clone())
        );
        let stream = make_host_stream();
        state.host_streams.update(|streams| {
            streams.insert(b.local_host_id.clone(), stream.clone());
        });
        dispatch_envelope(
            &state,
            &b.local_host_id,
            protocol::Envelope::from_payload(
                stream,
                FrameKind::Welcome,
                0,
                &protocol::WelcomePayload {
                    mobile_client_bootstrap: None,
                    protocol_version: protocol::PROTOCOL_VERSION,
                    tyde_version: protocol::TYDE_VERSION,
                    release_version: Some(
                        protocol::TydeReleaseVersion::parse("0.9.4-beta.11").unwrap(),
                    ),
                },
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(loader_owns_target(&b.local_host_id.0, "0.9.4-beta.11"));
        apply_paired_hosts_list(&state, vec![a.clone(), c]);
        next_tick().await;
        assert!(
            loader_no_target(),
            "forgetting the owner must retire its cold-boot target"
        );
        apply_connection_status(
            &state,
            b.local_host_id.clone(),
            PairedHostConnectionStatus::Connected,
            Some(102),
        );
        next_tick().await;
        assert!(container.text_content().unwrap().contains("Pick a Host"));
        assert!(state.active_local_host_id.get_untracked().is_none());
        apply_paired_hosts_list(&state, vec![a.clone()]);
        next_tick().await;
        assert!(container.text_content().unwrap().contains("First host"));
        assert_eq!(
            state.active_local_host_id.get_untracked(),
            Some(a.local_host_id)
        );
        drop(handle);
        container.remove();
        restore_loader_boundary();

        // Pair completion must use its created ID, not the old selected record or a
        // same-named row. Exercise the real Pair action, store and replay boundary.
        use crate::bridge::web::tests_support::{
            DirectPairingRecord, IndexedDbHostStore, WebPairedHostRecord,
        };
        use mobile_shell_types::native::{NATIVE_BRIDGE_VERSION, NativeIntent};
        for native in [true, false] {
            if native {
                install_loader_boundary();
            }
            let _sends = bridge::test_capture_sends();
            let previous_origin = "https://previous-host.example.test";
            let old = WebPairedHostRecord {
                local_host_id: LocalHostId(uuid::Uuid::new_v4().to_string()),
                host_label: "Tyde Host".into(),
                credential_fingerprint: "pair-selection-previous".into(),
                auto_connect: false,
                last_connected_at_ms: None,
                psk_keychain_key_id: None,
                managed: None,
                direct: Some(DirectPairingRecord {
                    native_origin: Some(previous_origin.into()),
                    device_id: uuid::Uuid::new_v4().to_string(),
                    device_token_key_id: mobile_shell_types::KeychainSecretId(
                        "pair-selection-old-token".into(),
                    ),
                }),
            };
            IndexedDbHostStore.insert(old.clone()).await.unwrap();
            let grant = protocol::MobileDirectPairResponse {
                device_id: protocol::MobileDeviceId(uuid::Uuid::new_v4().to_string()),
                device_token: protocol::MobileDeviceToken("pair-selection-test-token".into()),
                host_label: "Tyde Host".into(),
                protocol_version: protocol::PROTOCOL_VERSION,
            };
            let offer = mobile_pairing::DirectMobilePairingQrPayload::new(
                protocol::PROTOCOL_VERSION,
                protocol::TydeReleaseVersion::parse(
                    mobile_shell_types::native::NATIVE_CLIENT_RELEASE,
                )
                .unwrap(),
                protocol::MobilePairingOfferId::new(uuid::Uuid::new_v4().to_string()).unwrap(),
                "pair-selection-test-offer".into(),
                grant.host_label.clone(),
                u64::MAX,
            );
            let uri = format!("https://127.0.0.1:9/tyde/#{}", offer.to_uri().unwrap());
            let managed_pairing_id = uuid::Uuid::new_v4().to_string();
            let managed_device_id = uuid::Uuid::new_v4().to_string();
            let managed_response = serde_json::json!({
                "pairing_id": managed_pairing_id, "device_id": managed_device_id,
                "device_pairing_secret": "pair-selection-managed-fixture-secret"
            });
            let failed_pairing_id = uuid::Uuid::new_v4().to_string();
            let failed_device_id = uuid::Uuid::new_v4().to_string();
            let failed_managed_response = serde_json::json!({
                "pairing_id": failed_pairing_id, "device_id": failed_device_id,
                "device_pairing_secret": "pair-selection-managed-failed-read-secret"
            });
            let boundary = js_sys::Function::new_with_args("enabled,version,body,managedBody", r#"
                const fetch = window.fetch;
                const hadNative = Object.hasOwn(window, '__tydeNativeHost');
                const previous = window.__tydeNativeHost;
                const selection = localStorage.getItem('tyde.selected-host.v1');
                const hadService = Object.hasOwn(window, '__TYDE_MOBILE_SERVICE__');
                const previousService = window.__TYDE_MOBILE_SERVICE__;
                let redeemStarted, releaseRedeem;
                const redeemReady = new Promise(resolve => redeemStarted = resolve);
                const secrets = new Map();
                const deniedReads = new Map();
                const native = {version, holdReads: true, failReads: false, pendingReads: [], post(endpoint, payload) {
                    const request = JSON.parse(payload);
                    if (endpoint === 'presentation') { this.presentation = request; return Promise.resolve(null); }
                    if (endpoint === 'secret_write') { secrets.set(request.key, request.value); return Promise.resolve(null); }
                    if (endpoint === 'secret_delete') { secrets.delete(request.key); return Promise.resolve(null); }
                    if (endpoint === 'secret_read') {
                        if (!secrets.has(request.key)) return Promise.reject(new Error('Fixture secret key is missing'));
                        if (this.failReads) {
                            deniedReads.set(request.key, (deniedReads.get(request.key) || 0) + 1);
                            return Promise.reject(new Error('Platform denied the secret read'));
                        }
                        // Direct token reads are downstream of actor startup; managed PSK reads are not.
                        if (this.holdReads) return new Promise(resolve => this.pendingReads.push(resolve));
                        return Promise.resolve(secrets.get(request.key));
                    }
                    return Promise.reject(new Error('Unexpected pairing fixture platform operation'));
                }};
                if (enabled) window.__tydeNativeHost = native;
                else delete window.__tydeNativeHost;
                window.fetch = request => {
                    const path = new URL(request.url).pathname;
                    if (request.method === 'POST' && path === '/tyde/pair') {
                        return Promise.resolve(new Response(body, {status:200, headers:{'content-type':'application/json'}}));
                    }
                    if (request.method === 'POST' && path === '/api/tyde/mobile/v1/pairings/redeem') {
                        return new Promise(resolve => {
                            releaseRedeem = () => resolve(new Response(managedBody, {status:200}));
                            redeemStarted();
                        });
                    }
                    if (request.method === 'POST' && path.endsWith('/webrtc')) {
                        return Promise.resolve(new Response(JSON.stringify({error:{code:'repair_required',message:'Fixture has no relay',retryable:false}}), {status:403}));
                    }
                    return Promise.reject(new Error('Unexpected pairing fixture HTTP request'));
                };
                const restore = () => {
                    window.fetch = fetch;
                    if (hadNative) window.__tydeNativeHost = previous;
                    else delete window.__tydeNativeHost;
                    if (selection === null) localStorage.removeItem('tyde.selected-host.v1');
                    else localStorage.setItem('tyde.selected-host.v1', selection);
                    if (hadService) window.__TYDE_MOBILE_SERVICE__ = previousService;
                    else delete window.__TYDE_MOBILE_SERVICE__;
                };
                restore.deniedReads = deniedReads;
                restore.beginManaged = (response, failReads) => {
                    managedBody = response;
                    restore.redeemReady = new Promise(resolve => redeemStarted = resolve);
                    native.holdReads = false;
                    native.failReads = failReads;
                    window.__TYDE_MOBILE_SERVICE__ = {baseUrl:'https://pair-selection.example.test/api/tyde/mobile/v1'};
                };
                restore.releaseManaged = () => releaseRedeem();
                return restore;
            "#).call4(&wasm_bindgen::JsValue::NULL, &native.into(), &NATIVE_BRIDGE_VERSION.into(),
                &serde_json::to_string(&grant).unwrap().into(), &managed_response.to_string().into()).unwrap();
            let boundary = BrowserOverride(boundary);
            let state = AppState::new();
            state.paired_hosts.set(vec![old.summary()]);
            state
                .active_local_host_id
                .set(Some(old.local_host_id.clone()));
            state.active_tab.set(MobileTab::Settings);
            if native {
                state.active_agent.set(Some(crate::state::ActiveAgentRef {
                    local_host_id: old.local_host_id.clone(),
                    agent_id: protocol::AgentId("old-host-chat".into()),
                }));
                state
                    .active_project
                    .set(Some(crate::state::ActiveProjectRef {
                        local_host_id: old.local_host_id.clone(),
                        project_id: protocol::ProjectId("previous-host-project".into()),
                    }));
                state
                    .native_chat_destination
                    .set(Some(crate::state::NativeChatDestination {
                        local_host_id: old.local_host_id.clone(),
                        stream: protocol::StreamPath(
                            "/host/previous/spawn/00000000-0000-0000-0000-000000000001".into(),
                        ),
                        tab: MobileTab::Sessions,
                        draft_id: state.chat_draft_id.get_untracked(),
                        source_agent: state.active_agent.get_untracked(),
                        created_agent: None,
                        source_draft: Some(state.chat_input.get_untracked()),
                    }));
                state
                    .draft_backend_override
                    .set(Some(protocol::BackendKind::Claude));
                state
                    .draft_custom_agent_id
                    .set(Some(protocol::CustomAgentId("previous-host-custom".into())));
                state
                    .draft_session_settings
                    .set(protocol::SessionSettingsValues(
                        [(
                            "model".into(),
                            protocol::SessionSettingValue::String("previous-host-model".into()),
                        )]
                        .into_iter()
                        .collect(),
                    ));
                state.chat_origin_tab.set(MobileTab::Agents);
                state.viewing_chat.set(true);
            }
            state.app_mode.set(AppMode::Pairing(
                crate::state::PairingScreen::SelfHostedConfirm {
                    qr_uri: uri,
                    host_label: grant.host_label.clone(),
                },
            ));
            let paired_state = state.clone();
            let paired = bridge::listen_paired_hosts_changed(move |event| {
                apply_paired_hosts_list(&paired_state, event.hosts);
            })
            .await
            .unwrap();
            let status_state = state.clone();
            let status = bridge::listen_paired_host_connection_status(move |event| {
                apply_connection_status(
                    &status_state,
                    event.local_host_id,
                    event.status,
                    event.connection_instance_id,
                );
            })
            .await
            .unwrap();
            let container = make_container();
            let mounted = state.clone();
            let handle = mount_to(container.clone(), move || {
                provide_context(mounted);
                view! { <AppSurface /> }
            });
            next_tick().await;
            let result: Result<(), String> = async {
                let mut ownership_failures = Vec::new();
                let complete = js_sys::Function::new_with_args("root", r#"
                    return new Promise(resolve => {
                        const observer = new MutationObserver(() => {
                            if (!root.querySelector('[data-mobile-test="pairing-self-hosted-confirm"], [data-mobile-test="pairing-self-hosted-progress"]')) {
                                observer.disconnect(); resolve();
                            }
                        });
                        observer.observe(root, {childList:true, subtree:true});
                    });
                "#).call1(&wasm_bindgen::JsValue::NULL, &container).unwrap();
                container.query_selector("button[aria-label='Pair']").unwrap()
                    .ok_or("The direct confirmation must offer its real Pair action")?
                    .dyn_into::<HtmlElement>().unwrap().click();
                wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(complete)).await.unwrap();
                if state.app_mode.get_untracked() != AppMode::Workspace {
                    return Err("The direct Pair action did not complete into its workspace".into());
                }
                let created = IndexedDbHostStore.list().await.unwrap().into_iter()
                    .find(|record| record.direct.as_ref().is_some_and(|direct| direct.device_id == grant.device_id.0))
                    .ok_or("The Pair action must persist the redeemed device before navigation")?;
                if native {
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&created.local_host_id)
                        || container.query_selector("[data-mobile-test='home-view']").unwrap().is_none() {
                        return Err("Native Pair must open the newly created host's Home, not the previously selected host".into());
                    }
                    if state.active_project.get_untracked().is_some() || state.active_agent.get_untracked().is_some()
                        || state.native_chat_destination.get_untracked().is_some()
                        || state.draft_backend_override.get_untracked().is_some() || state.draft_custom_agent_id.get_untracked().is_some()
                        || !state.draft_session_settings.get_untracked().0.is_empty() {
                        return Err("Pair must retire previous-host project, draft options, chat selection and pending UI navigation without deleting its records".into());
                    }
                    if matches!(state.active_host_connection_status(), ConnectionStatus::Connected) {
                        return Err("Pair completion must not fabricate Connected before Welcome/bootstrap".into());
                    }
                    apply_paired_hosts_list(&state, IndexedDbHostStore.list_summaries().await.unwrap());
                    next_tick().await;
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&created.local_host_id) {
                        return Err("Persisted host-list replay must not steal the explicit Pair selection".into());
                    }
                    let intent = |intent: NativeIntent| {
                        let options = web_sys::CustomEventInit::new();
                        options.set_detail(&serde_wasm_bindgen::to_value(&intent).unwrap());
                        web_sys::window().unwrap().dispatch_event(&web_sys::CustomEvent::new_with_event_init_dict("tyde-native-intent", &options).unwrap()).unwrap();
                    };
                    intent(NativeIntent::Hosts);
                    next_tick().await;
                    if container.query_selector(".picker-pair-another").unwrap().is_none()
                        || state.active_local_host_id.get_untracked().as_ref() != Some(&created.local_host_id) {
                        return Err("The native Hosts destination must expose Pair another host without clearing selection".into());
                    }
                    let platform = js_sys::Reflect::get(&web_sys::window().unwrap(), &"__tydeNativeHost".into()).unwrap();
                    let projected: mobile_shell_types::native::NativePresentation = serde_wasm_bindgen::from_value(
                        js_sys::Reflect::get(&platform, &"presentation".into()).unwrap()).unwrap();
                    if projected.route != mobile_shell_types::native::NativeRoute::Hosts || !projected.workspace {
                        return Err("The host chooser must project a native navigation detail, not hide system Back".into());
                    }
                    intent(NativeIntent::Back);
                    next_tick().await;
                    if state.app_mode.get_untracked() != AppMode::Workspace
                        || container.query_selector("[data-mobile-test='home-view']").unwrap().is_none() {
                        return Err("Native Back from Hosts must restore the selected host's Home".into());
                    }
                    intent(NativeIntent::Hosts);
                    next_tick().await;
                    container.query_selector(".picker-pair-another").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    if container.query_selector("[data-mobile-test='pairing-scanner-paste']").unwrap().is_none() {
                        return Err("Pair another host must open the normal scanner/Paste link route".into());
                    }
                    container.query_selector("[data-mobile-test='pairing-scanner-cancel']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&created.local_host_id) {
                        return Err("Cancel pairing must preserve the selected host".into());
                    }
                    let paired_stream = protocol::StreamPath("/host/paired-selection".into());
                    state.host_streams.update(|streams| { streams.insert(created.local_host_id.clone(), paired_stream.clone()); });
                    state.sessions.set(vec![fixture_session(&created.local_host_id, "paired-pending-session")]);
                    intent(NativeIntent::Sessions);
                    next_tick().await;
                    container.query_selector("[data-mobile-test='session-resume']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    if state.native_chat_destination.get_untracked().as_ref().map(|target| &target.local_host_id)
                        != Some(&created.local_host_id) {
                        return Err("A real Resume must retain its typed pending destination before host selection".into());
                    }
                    state.active_project.set(Some(crate::state::ActiveProjectRef {
                        local_host_id: created.local_host_id.clone(), project_id: protocol::ProjectId("paired-host-project".into()),
                    }));
                    state.draft_backend_override.set(Some(protocol::BackendKind::Claude));
                    state.draft_custom_agent_id.set(Some(protocol::CustomAgentId("paired-host-custom".into())));
                    state.draft_session_settings.set(protocol::SessionSettingsValues(
                        [("model".into(), protocol::SessionSettingValue::String("paired-host-model".into()))].into_iter().collect()));
                    state.active_agent.set(Some(crate::state::ActiveAgentRef {
                        local_host_id: created.local_host_id.clone(), agent_id: protocol::AgentId("new-host-chat".into()),
                    }));
                    state.chat_origin_tab.set(MobileTab::Agents);
                    state.viewing_chat.set(true);
                    intent(NativeIntent::Agents);
                    next_tick().await;
                    let previous_composer = container.query_selector("[data-mobile-test='chat-input']").unwrap().unwrap()
                        .dyn_into::<web_sys::HtmlTextAreaElement>().unwrap();
                    previous_composer.set_value("Unsent text belongs to the current host");
                    previous_composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
                    intent(NativeIntent::Hosts);
                    next_tick().await;
                    let rows = container.query_selector_all(".paired-host-card").unwrap();
                    let rows = (0..rows.length()).map(|index| rows.item(index).unwrap().dyn_into::<web_sys::Element>().unwrap()).collect::<Vec<_>>();
                    // Store events replay all records. Assert uniqueness for both exact owned identities,
                    // rather than assuming unrelated flows left the shared database empty.
                    let mut previous_open = None;
                    for record in [&old, &created] {
                        let origin = record.direct.as_ref().and_then(|direct| direct.native_origin.as_deref())
                            .ok_or("A native direct pairing must preserve its validated origin")?;
                        let matches = rows.iter().filter(|row| {
                            row.query_selector("[data-mobile-test='paired-host-origin']").unwrap()
                                .is_some_and(|text| text.text_content().as_deref() == Some(origin))
                            && row.query_selector("[data-mobile-test='paired-host-identity']").unwrap()
                                .is_some_and(|identity| identity.text_content().unwrap().contains(&record.credential_fingerprint))
                        }).collect::<Vec<_>>();
                        if matches.len() != 1 {
                            return Err("Each of the two exact persisted same-label host identities must have exactly one visible chooser row".into());
                        }
                        let row = matches[0];
                        let open = row.query_selector("[data-mobile-test='paired-host-open']").unwrap().unwrap();
                        let delete = row.query_selector("[data-mobile-test='paired-host-forget']").unwrap().unwrap();
                        let identity = format!("{}, {origin}, {}", record.host_label, record.credential_fingerprint);
                        if row.query_selector(".paired-host-label").unwrap().unwrap().text_content().as_deref() != Some("Tyde Host")
                            || open.get_attribute("aria-label").as_deref() != Some(format!("Open {identity}").as_str())
                            || delete.get_attribute("aria-label").as_deref() != Some(format!("Delete pairing for {identity}").as_str()) {
                            return Err("Both same-label host rows need exact, distinct visible identities and accessible Open/Delete actions".into());
                        }
                        if record.local_host_id == old.local_host_id { previous_open = Some(open); }
                    }
                    let open = previous_open.ok_or("The previous exact host must remain normally selectable")?.dyn_into::<HtmlElement>().unwrap();
                    open.click();
                    next_tick().await;
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&created.local_host_id) || !previous_composer.is_connected() || state.chat_input.get_untracked().is_empty() {
                        return Err("A host switch must not erase an unsent mounted draft before loader safety is evaluated".into());
                    }
                    // Explicitly finish/discard this fixture's owned draft before exercising the original cross-host routing guarantee.
                    state.chat_input.set(String::new());
                    state.draft_session_settings.set(Default::default());
                    state.draft_backend_override.set(None);
                    state.draft_custom_agent_id.set(None);
                    state.viewing_chat.set(false);
                    state.mobile_shell_error.set(None);
                    open.click();
                    next_tick().await;
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&old.local_host_id)
                        || state.app_mode.get_untracked() != AppMode::Workspace
                        || container.query_selector("[data-mobile-test='home-view']").unwrap().is_none() {
                        return Err("Open in the real chooser must use that exact record and restore Home".into());
                    }
                    if state.active_project.get_untracked().is_some() || state.native_chat_destination.get_untracked().is_some()
                        || state.active_agent.get_untracked().is_some() {
                        return Err("Cross-host Open must retire the previous project's and pending resume's UI ownership".into());
                    }
                    reset_inbound_seq_for_host(&created.local_host_id);
                    let late = fixture_user_agent("late-paired-resume", "paired-pending-session");
                    dispatch_envelope(&state, &created.local_host_id,
                        protocol::Envelope::from_payload(paired_stream, FrameKind::NewAgent, 0, &late).unwrap());
                    next_tick().await;
                    if !state.agents.get_untracked().iter().any(|agent| agent.local_host_id == created.local_host_id && agent.agent_id == late.agent_id)
                        || state.active_local_host_id.get_untracked().as_ref() != Some(&old.local_host_id)
                        || state.active_agent.get_untracked().is_some() || state.viewing_chat.get_untracked() {
                        return Err("A delayed server resume must retain its host's agent data without stealing the explicit other-host selection".into());
                    }
                    let previous_stream = protocol::StreamPath("/host/previous-selection".into());
                    state.host_streams.update(|streams| { streams.insert(old.local_host_id.clone(), previous_stream.clone()); });
                    state.sessions.set(vec![fixture_session(&old.local_host_id, "previous-resume")]);
                    intent(NativeIntent::Sessions);
                    next_tick().await;
                    container.query_selector("[data-mobile-test='session-resume']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    let sent = bridge::test_sent_lines().into_iter().filter_map(|line| serde_json::from_str::<protocol::Envelope>(&line).ok())
                        .filter(|frame| frame.kind == FrameKind::SpawnAgent && frame.stream.spawn_parent().as_ref() == Some(&previous_stream)).collect::<Vec<_>>();
                    if sent.len() != 1 || sent[0].parse_payload::<protocol::SpawnAgentPayload>().unwrap().project_id.is_some() {
                        return Err("Resume must send through the selected host's real protocol seam without a different host's project".into());
                    }
                    intent(NativeIntent::Agents);
                    next_tick().await;
                    if state.viewing_chat.get_untracked() || container.query_selector("[data-mobile-test='chat-input']").unwrap()
                        .is_some_and(|field| field.get_bounding_client_rect().height() > 0.0) {
                        return Err("Opening another host must not expose or route Send to the previous host's conversation".into());
                    }
                    let existing = fixture_user_agent("other-host-existing-chat", "other-host-existing-session");
                    dispatch_envelope(&state, &old.local_host_id,
                        protocol::Envelope::from_payload(previous_stream.clone(), FrameKind::NewAgent, 0, &existing).unwrap());
                    state.connection_statuses.update(|statuses| { statuses.insert(old.local_host_id.clone(), ConnectionStatus::Connected); });
                    next_tick().await;
                    container.query_selector("[role='button'][aria-label='Open chat with other-host-existing-chat']").unwrap()
                        .ok_or("The selected host's existing conversation must be normally openable")?
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    let other_composer = container.query_selector("[data-mobile-test='chat-input']").unwrap().unwrap()
                        .dyn_into::<web_sys::HtmlTextAreaElement>().unwrap();
                    if !other_composer.value().is_empty() {
                        ownership_failures.push("Opening an existing conversation on a different host must not inherit the previous host's unsent text");
                    }
                    if !crate::native::can_leave_workspace(&state) {
                        return Err("The empty existing conversation must be safe before Rename opens".into());
                    }
                    intent(NativeIntent::RenameAgent);
                    next_tick().await;
                    let rename = container.query_selector("[data-mobile-test='chat-rename-input']").unwrap()
                        .ok_or("Native Rename must expose the actual retained edit")?
                        .dyn_into::<web_sys::HtmlInputElement>().unwrap();
                    rename.set_value("Retained rename text");
                    rename.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
                    intent(NativeIntent::Home);
                    next_tick().await;
                    if crate::native::can_leave_workspace(&state) || !rename.is_connected() {
                        return Err("Native Home must retain Rename and keep the client unsafe to replace".into());
                    }
                    intent(NativeIntent::Hosts);
                    next_tick().await;
                    let destination = created.direct.as_ref().unwrap().native_origin.as_deref().unwrap();
                    let cards = container.query_selector_all(".paired-host-card").unwrap();
                    let target = (0..cards.length()).map(|index| cards.item(index).unwrap().dyn_into::<web_sys::Element>().unwrap())
                        .find(|row| row.query_selector("[data-mobile-test='paired-host-origin']").unwrap()
                            .is_some_and(|node| node.text_content().as_deref() == Some(destination)))
                        .ok_or("The other exact persisted host must remain in the chooser")?;
                    target.query_selector("[data-mobile-test='paired-host-open']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    if state.active_local_host_id.get_untracked().as_ref() != Some(&old.local_host_id)
                        || !rename.is_connected() || rename.value() != "Retained rename text" {
                        return Err("An explicit cross-host Open must not discard the retained Rename edit".into());
                    }
                    intent(NativeIntent::Agents);
                    next_tick().await;
                    let retained_rename = container.query_selector("[data-mobile-test='chat-rename-input']").unwrap()
                        .ok_or("Returning to Agents must restore the same Rename edit")?;
                    if !retained_rename.is_same_node(Some(&rename)) {
                        return Err("The Rename input owner must not remount across native tabs/Hosts".into());
                    }
                    container.query_selector("[data-mobile-test='chat-rename-cancel']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    if !crate::native::can_leave_workspace(&state) {
                        return Err("Explicit Rename Cancel must release its unsaved-work fence".into());
                    }
                    state.mobile_shell_error.set(None);
                    other_composer.set_value("Explicit text for this host only");
                    other_composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
                    next_tick().await;
                    container.query_selector("[data-mobile-test='chat-send']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    // The primary send action is InterruptAndSend, carried by HumanInput.
                    let sent = bridge::test_sent_lines().into_iter().filter_map(|line| serde_json::from_str::<protocol::Envelope>(&line).ok())
                        .filter(|frame| frame.kind == FrameKind::HumanInput).collect::<Vec<_>>();
                    if sent.len() != 1 || sent[0].stream != existing.instance_stream
                        || sent[0].parse_payload::<protocol::HumanInputPayload>().unwrap().input.message != "Explicit text for this host only"
                        || sent[0].parse_payload::<protocol::HumanInputPayload>().unwrap().action != protocol::HumanInputAction::InterruptAndSend {
                        ownership_failures.push("The primary send action in the normally opened conversation must address the exact selected host's agent stream with only its explicit text");
                    }
                    intent(NativeIntent::Back);
                    next_tick().await;
                    let mut settings = empty_host_settings();
                    settings.default_backend = Some(protocol::BackendKind::Codex);
                    settings.enabled_backends = vec![protocol::BackendKind::Codex];
                    state.host_settings_by_host.update(|hosts| { hosts.insert(old.local_host_id.clone(), settings); });
                    state.connection_statuses.update(|statuses| { statuses.insert(old.local_host_id.clone(), ConnectionStatus::Connected); });
                    intent(NativeIntent::NewChat);
                    next_tick().await;
                    container.query_selector("[data-mobile-test='project-picker-none']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    let backend = container.query_selector("[data-mobile-test='new-chat-backend']").unwrap().unwrap()
                        .dyn_into::<web_sys::HtmlSelectElement>().unwrap();
                    let agent = container.query_selector("[data-mobile-test='new-chat-agent']").unwrap().unwrap()
                        .dyn_into::<web_sys::HtmlSelectElement>().unwrap();
                    let settings = container.query_selector("[data-mobile-test='new-chat-session-settings']").unwrap().unwrap();
                    if !backend.value().is_empty() || agent.value() != "tyde-default"
                        || !settings.text_content().unwrap().contains("Backend defaults") {
                        return Err("The next host's displayed composer options must not contain previous-host configuration".into());
                    }
                    let composer = container.query_selector("[data-mobile-test='chat-input']").unwrap().unwrap()
                        .dyn_into::<web_sys::HtmlTextAreaElement>().unwrap();
                    composer.set_value("A separate host's new conversation");
                    composer.dispatch_event(&web_sys::Event::new("input").unwrap()).unwrap();
                    next_tick().await;
                    container.query_selector("[data-mobile-test='chat-send']").unwrap().unwrap()
                        .dyn_into::<HtmlElement>().unwrap().click();
                    next_tick().await;
                    let frames = bridge::test_sent_lines().into_iter().filter_map(|line| serde_json::from_str::<protocol::Envelope>(&line).ok())
                        .filter(|frame| frame.kind == FrameKind::SpawnAgent && frame.stream.spawn_parent().as_ref() == Some(&previous_stream)).collect::<Vec<_>>();
                    let payload = frames.last().ok_or("Send must cross the real protocol-send seam")?
                        .parse_payload::<protocol::SpawnAgentPayload>().unwrap();
                    if frames.len() != 2 || payload.custom_agent_id.is_some() || payload.project_id.is_some()
                        || !matches!(payload.params, protocol::SpawnAgentParams::New { backend_kind: protocol::BackendKind::Codex, session_settings: None, .. }) {
                        return Err("The next host's real SpawnAgent must not inherit a different host's custom agent, backend, model or project".into());
                    }
                } else if state.active_local_host_id.get_untracked().as_ref() != Some(&old.local_host_id)
                    || state.active_tab.get_untracked() != MobileTab::Settings
                    || container.query_selector("[data-mobile-test='bottom-nav']").unwrap().is_none()
                    || container.query_selector("[data-mobile-test='paired-host-origin']").unwrap().is_some() {
                    return Err("Native Pair navigation must not change the browser/PWA selection, tab or global navigation".into());
                }
                let selected_before_managed = state.active_local_host_id.get_untracked();
                let tab_before_managed = state.active_tab.get_untracked();
                complete_managed_pair_fixture(&state, &container, &boundary, &managed_response, false).await?;
                let managed = IndexedDbHostStore.list().await.unwrap().into_iter()
                    .find(|record| record.managed.as_ref().is_some_and(|managed| managed.pairing_id == managed_pairing_id && managed.device_id == managed_device_id))
                    .ok_or("Managed success must persist the exact redeemed identity before native navigation")?;
                if native {
                    if state.app_mode.get_untracked() != AppMode::Workspace
                        || state.active_local_host_id.get_untracked().as_ref() != Some(&managed.local_host_id)
                        || state.active_tab.get_untracked() != MobileTab::Home
                        || state.active_project.get_untracked().is_some() || state.native_chat_destination.get_untracked().is_some()
                        || container.query_selector("[data-mobile-test='home-view']").unwrap().is_none() {
                        return Err("Managed Pair completion must use its exact created typed record and the same native Home/ownership transition as direct Pair".into());
                    }
                    if matches!(state.active_host_connection_status(), ConnectionStatus::Connected) {
                        return Err("Managed Pair selection must not invent a completed relay/bootstrap connection".into());
                    }
                    let stopped = js_sys::Function::new_with_args("root", r#"
                        return new Promise(resolve => {
                            const settled = () => root.querySelector('[data-mobile-test="connection-banner-needs-action"]');
                            if (settled()) { resolve(); return; }
                            const observer = new MutationObserver(() => {
                                if (settled()) { observer.disconnect(); resolve(); }
                            });
                            observer.observe(root, {childList:true, subtree:true});
                        });
                    "#).call1(&wasm_bindgen::JsValue::NULL, &container).unwrap();
                    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(stopped)).await.unwrap();
                } else if state.active_local_host_id.get_untracked() != selected_before_managed
                    || state.active_tab.get_untracked() != tab_before_managed
                    || container.query_selector("[data-mobile-test='bottom-nav']").unwrap().is_none() {
                    return Err("Returning a managed pairing identity must not alter the browser's existing selection/tab/navigation behavior".into());
                }
                if native {
                    let selected = state.active_local_host_id.get_untracked();
                    complete_managed_pair_fixture(&state, &container, &boundary, &failed_managed_response, true).await?;
                    let saved = IndexedDbHostStore.list().await.unwrap().into_iter()
                        .find(|record| record.managed.as_ref().is_some_and(|managed| managed.pairing_id == failed_pairing_id && managed.device_id == failed_device_id))
                        .ok_or("A pre-actor Keychain failure must preserve the saved pairing for explicit recovery")?;
                    let failure = container.query_selector("[data-mobile-test='pairing-failed-message']").unwrap();
                    if !matches!(state.app_mode.get_untracked(), AppMode::Pairing(crate::state::PairingScreen::Failed { .. }))
                        || failure.as_ref().and_then(|message| message.text_content()).as_deref()
                            != Some("Pairing was saved, but connection could not start: Native secret_read failed: Platform denied the secret read")
                        || state.active_local_host_id.get_untracked() != selected
                        || !state.paired_hosts.get_untracked().iter().any(|host| host.local_host_id == saved.local_host_id) {
                        return Err("Managed pre-actor platform failure must stay actionable, retain its pairing, and never navigate as a successful connection".into());
                    }
                    // Re-enter the production attach path with the persisted auto-connect record.
                    state.mobile_shell_error.set(None);
                    state.app_mode.set(AppMode::Workspace);
                    let denied_reads = js_sys::Reflect::get(&boundary.0, &"deniedReads".into()).unwrap().dyn_into::<js_sys::Map>().unwrap();
                    denied_reads.clear();
                    install_event_listeners(state.clone()).await;
                    next_tick().await;
                    let error = container.query_selector("[data-mobile-test='mobile-shell-error-banner']").unwrap();
                    let message = container.query_selector(".mobile-shell-error-message").unwrap();
                    let diagnostics = format!("Startup failure: banner={}, code={:?}, message_present={}, denied_read_count={:?}, expected_prefix={:?}, saved_auto_connect={}",
                        error.is_some(), error.as_ref().and_then(|banner| banner.get_attribute("data-error-code")),
                        message.is_some(), message.as_ref().and_then(|message| message.text_content()).map(|text| text.matches("Native secret_read failed: Platform denied the secret read").count()),
                        message.as_ref().and_then(|message| message.text_content()).map(|text| text.starts_with("Failed to refresh host connection after app attach: ")),
                        saved.auto_connect);
                    web_sys::console::log_1(&diagnostics.into());
                    // The real run reports one denied read: native attach now owns only
                    // the selected host. Retained inactive pairings must not be started.
                    if error.as_ref().and_then(|banner| banner.get_attribute("data-error-code")).as_deref() != Some("Internal")
                        || !message.as_ref().and_then(|message| message.text_content()).is_some_and(|text|
                            text.starts_with("Failed to refresh host connection after app attach: ")
                            && text.matches("Native secret_read failed: Platform denied the secret read").count() == 1)
                        || state.active_local_host_id.get_untracked().as_ref() != Some(&managed.local_host_id)
                        || denied_reads.size() != 1
                        || denied_reads.get(&managed.psk_keychain_key_id.as_ref().unwrap().0.clone().into()).as_f64() != Some(1.0) {
                        ownership_failures.push("Only the selected managed host's denied startup read must surface; inactive pairing secrets must remain unread");
                    }
                    state.mobile_shell_error.set(None);
                    state.active_local_host_id.set(Some(saved.local_host_id.clone()));
                    state.connection_statuses.update(|statuses| { statuses.insert(saved.local_host_id.clone(), ConnectionStatus::Disconnected); });
                    next_tick().await;
                    let reconnect = container.query_selector("[data-mobile-test='connection-banner-reconnect']").unwrap()
                        .ok_or("The saved disconnected host must offer the normal Reconnect action")?;
                    let completed = js_sys::Function::new_with_args("button", r#"
                        return new Promise(resolve => {
                            const observer = new MutationObserver(records => {
                                if (!button.disabled && records.some(record => record.oldValue !== null)) {
                                    observer.disconnect(); resolve();
                                }
                            });
                            observer.observe(button, {attributes:true, attributeFilter:['disabled'], attributeOldValue:true});
                            button.click();
                        });
                    "#).call1(&wasm_bindgen::JsValue::NULL, &reconnect).unwrap();
                    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(completed)).await.unwrap();
                    next_tick().await;
                    let failure = container.query_selector(".mobile-shell-error-message").unwrap();
                    if failure.as_ref().and_then(|message| message.text_content()).as_deref()
                        != Some("Could not reconnect: Native secret_read failed: Platform denied the secret read") {
                        ownership_failures.push("The real Reconnect action must show its pre-actor native credential failure after command completion");
                    }
                }
                if !ownership_failures.is_empty() { return Err(ownership_failures.join("; ")); }
                Ok(())
            }.await;
            paired.remove();
            status.remove();
            drop(handle);
            container.remove();
            for record in IndexedDbHostStore.list().await.unwrap() {
                if record.local_host_id == old.local_host_id
                    || record
                        .direct
                        .as_ref()
                        .is_some_and(|direct| direct.device_id == grant.device_id.0)
                    || record.managed.as_ref().is_some_and(|managed| {
                        (managed.pairing_id == managed_pairing_id
                            && managed.device_id == managed_device_id)
                            || (managed.pairing_id == failed_pairing_id
                                && managed.device_id == failed_device_id)
                    })
                {
                    if native {
                        // This is fixture teardown after unmount, not a user Delete.
                        // Native Delete is bootstrap-owned and covered across real
                        // documents in the retained-shell server/DOM scenario.
                        IndexedDbHostStore
                            .remove(&record.local_host_id)
                            .await
                            .unwrap();
                    } else {
                        bridge::forget_paired_host(&record.local_host_id)
                            .await
                            .unwrap();
                    }
                }
            }
            drop(boundary);
            if native {
                restore_loader_boundary();
            }
            assert!(result.is_ok(), "{}", result.unwrap_err());
        }
    }

    async fn complete_managed_pair_fixture(
        state: &AppState,
        container: &HtmlElement,
        boundary: &BrowserOverride,
        response: &serde_json::Value,
        fail_reads: bool,
    ) -> Result<(), String> {
        let begin = js_sys::Reflect::get(&boundary.0, &"beginManaged".into())
            .unwrap()
            .dyn_into::<js_sys::Function>()
            .unwrap();
        begin
            .call2(
                &wasm_bindgen::JsValue::NULL,
                &response.to_string().into(),
                &fail_reads.into(),
            )
            .unwrap();
        let mut offer = bridge::web::tests_support::sample_managed_payload();
        offer.host_label = "Tyde Host".into();
        offer.release_version =
            protocol::TydeReleaseVersion::parse(mobile_shell_types::native::NATIVE_CLIENT_RELEASE)
                .unwrap();
        state
            .app_mode
            .set(AppMode::Pairing(crate::state::PairingScreen::ServiceAuth {
                qr_uri: offer.to_uri().unwrap(),
                host_label: offer.host_label.clone(),
                auth: MobileServiceAuthState::Authenticated {
                    expires_at_ms: u64::MAX,
                },
            }));
        let ready = js_sys::Reflect::get(&boundary.0, &"redeemReady".into()).unwrap();
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(ready))
            .await
            .unwrap();
        let completed = js_sys::Function::new_with_args("root", r#"
            if (!root.querySelector('[data-mobile-test="pairing-service-cancel"]')) throw new Error('Managed redemption must render its real in-flight screen');
            return new Promise(resolve => {
                const observer = new MutationObserver(() => {
                    if (!root.querySelector('[data-mobile-test="pairing-service-cancel"]')) {
                        observer.disconnect(); resolve();
                    }
                });
                observer.observe(root, {childList:true, subtree:true});
            });
        "#).call1(&wasm_bindgen::JsValue::NULL, container).map_err(|_| "Managed in-flight view is missing")?;
        let release = js_sys::Reflect::get(&boundary.0, &"releaseManaged".into())
            .unwrap()
            .dyn_into::<js_sys::Function>()
            .unwrap();
        release.call0(&wasm_bindgen::JsValue::NULL).unwrap();
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(completed))
            .await
            .unwrap();
        Ok(())
    }

    struct BrowserOverride(wasm_bindgen::JsValue);

    impl Drop for BrowserOverride {
        fn drop(&mut self) {
            self.0
                .unchecked_ref::<js_sys::Function>()
                .call0(&wasm_bindgen::JsValue::NULL)
                .unwrap();
        }
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

    fn empty_host_settings() -> settings_model::HostSettings {
        settings_model::HostSettings {
            workbenches: Default::default(),
            resume_previous_agents: settings_model::default_resume_previous_agents(),
            pause_swarms_on_restart: false,
            review: Default::default(),
            self_improvement: Default::default(),
            enabled_backends: Vec::new(),
            default_backend: None,
            default_launch_profile_id: None,
            enable_mobile_connections: false,

            mobile_direct_hosting_enabled: false,
            mobile_direct_bind_addr: None,
            mobile_direct_public_origin: None,
            mobile_direct_bundle_dir: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
            delegation_launch_profile_order:
                settings_model::default_delegation_launch_profile_order(),
            complexity_tiers_enabled: false,
            backend_tier_configs: Default::default(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
            usage_limits: Default::default(),
            code_intel: Default::default(),
            backend_config: Default::default(),
            launch_profiles: Default::default(),
            hermes_disabled_providers: Default::default(),
            voice: Default::default(),
            tychat: Default::default(),
            chatgpt_dots_enabled: false,
        }
    }

    fn fixture_user_agent(id: &str, session_id: &str) -> protocol::NewAgentPayload {
        protocol::NewAgentPayload {
            swarm_membership: None,
            agent_id: protocol::AgentId(id.into()),
            name: id.into(),
            origin: protocol::AgentOrigin::User,
            tyde_permissions: Default::default(),
            backend_kind: protocol::BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: Vec::new(),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: Some(protocol::SessionId(session_id.into())),
            workflow: None,
            created_at_ms: 0,
            instance_stream: protocol::StreamPath(format!("/agent/{id}/instance")),
            activity_summary: Default::default(),
            activity: protocol::AgentActivity::Idle,
            review_recipient: false,
        }
    }

    fn fixture_session(host: &LocalHostId, id: &str) -> crate::state::SessionInfo {
        crate::state::SessionInfo {
            local_host_id: host.clone(),
            summary: protocol::SessionSummary {
                id: protocol::SessionId(id.into()),
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                workspace_roots: Vec::new(),
                project_id: None,
                alias: Some("A resumable session".into()),
                user_alias: None,
                parent_id: None,
                created_at_ms: 0,
                updated_at_ms: 0,
                message_count: 0,
                token_count: None,
                resumable: true,
                compacted_from_session_id: None,
                compacted_to_session_id: None,
                compacted_at_ms: None,
                compaction_summary_preview: None,
            },
        }
    }

    fn fixture_host(id: &str, label: &str) -> PairedHostSummary {
        PairedHostSummary {
            local_host_id: LocalHostId(id.to_owned()),
            host_label: label.to_owned(),

            direct_origin: None,
            credential_fingerprint: "fp".to_owned(),
            auto_connect: false,
            last_connected_at_ms: None,
        }
    }

    #[wasm_bindgen_test]
    async fn one_chat_mount_preserves_transcript_and_composer_draft() {
        let document = web_sys::window().unwrap().document().unwrap();
        let frame = document
            .create_element("iframe")
            .unwrap()
            .dyn_into::<web_sys::HtmlIFrameElement>()
            .unwrap();
        frame
            .set_attribute("style", "width:393px;height:852px;border:0")
            .unwrap();
        document.body().unwrap().append_child(&frame).unwrap();
        async fn render_shell(frame: &web_sys::HtmlIFrameElement) {
            next_tick().await;
            for _ in 0..2 {
                let rendered = js_sys::Promise::new(&mut |resolve, _reject| {
                    frame
                        .content_window()
                        .unwrap()
                        .request_animation_frame(&resolve)
                        .unwrap();
                });
                wasm_bindgen_futures::JsFuture::from(rendered)
                    .await
                    .unwrap();
            }
        }
        let frame_document = frame.content_document().unwrap();
        let style = frame_document.create_element("style").unwrap();
        style.set_text_content(Some(concat!(
            include_str!("../vendor/web-shell/shell.css"),
            include_str!("../styles.css")
        )));
        frame_document.body().unwrap().append_child(&style).unwrap();
        let container = document
            .create_element("div")
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        frame_document
            .body()
            .unwrap()
            .append_child(&container)
            .unwrap();
        let host = LocalHostId("composer-host".to_owned());
        let agent_id = protocol::AgentId("composer-agent".to_owned());
        let agent_ref = crate::state::AgentRef {
            local_host_id: host.clone(),
            agent_id: agent_id.clone(),
        };
        let state = AppState::new();
        state.active_local_host_id.set(Some(host.clone()));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), ConnectionStatus::Connected);
        });
        state.host_settings_by_host.update(|settings| {
            settings.insert(host.clone(), empty_host_settings());
        });
        // Connected without a current-stream bootstrap correctly renders Home's
        // loading state, not the dashboard cards exercised below.
        let host_stream = protocol::StreamPath("/host/composer".to_owned());
        state.host_streams.update(|streams| {
            streams.insert(host.clone(), host_stream.clone());
        });
        state.bootstrapped_host_streams.update(|streams| {
            streams.insert(host.clone(), host_stream);
        });
        state.agents.set(vec![crate::state::AgentInfo {
            local_host_id: host.clone(),
            agent_id: agent_id.clone(),
            name: "Composer agent".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: protocol::StreamPath("/agent/composer-agent/instance".to_owned()),
            started: true,
            fatal_error: None,
            review_recipient: false,
        }]);
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: host,
            agent_id,
        }));
        state.agent_loaded.update(|loaded| {
            loaded.insert(agent_ref.clone());
        });
        state.push_chat_message_entry(
            &agent_ref,
            crate::state::ChatMessageEntry {
                message: protocol::ChatMessage {
                    origin: None,
                    message_id: None,
                    timestamp: 0,
                    sender: protocol::MessageSender::User,
                    content: "Preserve this transcript".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
                tool_requests: Vec::new(),
            },
        );
        let entry = state
            .chat_messages
            .with_untracked(|messages| messages[&agent_ref][0].clone());
        for _ in 0..40 {
            state.push_chat_message_entry(&agent_ref, entry.clone());
        }
        state.app_mode.set(AppMode::Workspace);
        state.viewing_chat.set(true);
        state.chat_input.set("preserved draft".to_owned());
        let state_for_mount = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <AppSurface /> }
        });
        next_tick().await;

        let transcript = container
            .query_selector("[data-mobile-test='chat-transcript']")
            .unwrap()
            .expect("one chat mount keeps the transcript visible");
        assert!(
            transcript
                .text_content()
                .unwrap_or_default()
                .contains("Preserve this transcript")
        );
        let input = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .expect("one chat mount keeps the composer visible")
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        assert_eq!(input.value(), "preserved draft");

        let send = container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .unwrap();
        let reachable = || {
            let rect = send.get_bounding_client_rect();
            frame_document
                .element_from_point(
                    (rect.x() + rect.width() / 2.0) as f32,
                    (rect.y() + rect.height() / 2.0) as f32,
                )
                .is_some_and(|hit| send.contains(Some(&hit)))
        };
        wasm_bindgen_test::console_log!(
            "Mounted shell geometry: shell={} send={} input={} content={}",
            container
                .query_selector(".mobile-app")
                .unwrap()
                .unwrap()
                .outer_html()
                .split('>')
                .next()
                .unwrap_or_default(),
            send.get_bounding_client_rect().bottom(),
            input.get_bounding_client_rect().bottom(),
            transcript.get_bounding_client_rect().bottom()
        );
        assert!(reachable(), "the shared shell composer is reachable");
        render_shell(&frame).await;
        let scroller: HtmlElement = container
            .query_selector("[data-mobile-test='chat-messages']")
            .unwrap()
            .unwrap()
            .unchecked_into();
        scroller.set_scroll_top(scroller.scroll_height());
        render_shell(&frame).await;
        let dock = container
            .query_selector("[data-mobile-test='chat-bottom-dock']")
            .unwrap()
            .unwrap();
        let shell = container.query_selector(".mobile-app").unwrap().unwrap();
        let assert_bottom_gap = |element: &web_sys::Element, expected: f64| {
            let gap = shell.get_bounding_client_rect().bottom()
                - element.get_bounding_client_rect().bottom();
            assert!(
                (gap - expected).abs() <= 1.0,
                "bottom chrome must keep the intended edge distance: expected={expected}, actual={gap}"
            );
        };
        // CSS safe-area inputs reproduce the measured iPhone spacing without
        // pretending an iframe supplies a physical keyboard or status bar.
        let safe_area = frame_document.create_element("style").unwrap();
        safe_area.set_text_content(Some(".mobile-app { --tws-safe-bottom: 34px; }"));
        frame_document
            .body()
            .unwrap()
            .append_child(&safe_area)
            .unwrap();
        render_shell(&frame).await;
        assert_bottom_gap(&dock, 26.0);
        let standalone_readings = || {
            BrowserOverride(js_sys::Function::new_with_args("w", r#"
                const entries = [[w.navigator, 'standalone', true],
                    [w.screen, 'height', 852],
                    [w.document.documentElement, 'clientHeight', 793],
                    [w.visualViewport, 'height', 793]];
                const clipStyle = w.document.createElement('style');
                clipStyle.textContent = 'html[data-tws-document], html[data-tws-document] body {height:793px}';
                w.document.body.append(clipStyle);
                const saved = entries.map(([object, key]) => Object.getOwnPropertyDescriptor(object, key));
                entries.forEach(([object, key, value]) => Object.defineProperty(object, key, {configurable:true, value}));
                w.dispatchEvent(new w.Event('resize'));
                return () => {
                    clipStyle.remove();
                    entries.forEach(([object, key], i) => {
                        if (saved[i]) Object.defineProperty(object, key, saved[i]);
                        else delete object[key];
                    });
                    w.dispatchEvent(new w.Event('resize'));
                };
            "#).call1(&wasm_bindgen::JsValue::NULL, &frame.content_window().unwrap()).unwrap())
        };
        let assert_physical_bottom = |element: &web_sys::Element, expected: f64| {
            let bottom = element.get_bounding_client_rect().bottom();
            wasm_bindgen_test::console_log!(
                "Standalone chrome bottom={bottom}, expected={expected}"
            );
            assert!(
                (bottom - expected).abs() <= 1.0,
                "standalone chrome reaches the physical edge: expected={expected}, actual={bottom}"
            );
        };
        let readings = standalone_readings();
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 59px; --tws-safe-bottom: 34px; }",
        ));
        render_shell(&frame).await;
        assert_physical_bottom(&dock, 852.0 - 26.0);
        for element in [
            frame_document.document_element().unwrap(),
            frame_document.body().unwrap().into(),
        ] {
            wasm_bindgen_test::console_log!(
                "Recovered paint surface: tag={}, height={}, inline_height={}, shell_height={}",
                element.tag_name(),
                element.get_bounding_client_rect().height(),
                element
                    .unchecked_ref::<HtmlElement>()
                    .style()
                    .get_property_value("height")
                    .unwrap(),
                shell.get_bounding_client_rect().height()
            );
            assert!(
                (element.get_bounding_client_rect().height() - 852.0).abs() <= 1.0,
                "the document paint surface must grow with the recovered shell"
            );
        }
        scroller.set_scroll_top(100);
        assert!(scroller.get_bounding_client_rect().top() >= 59.0);
        assert!(
            !frame_document
                .element_from_point(100.0, 30.0)
                .is_some_and(|hit| scroller.contains(Some(&hit))),
            "transcript is clipped below the status-bar clock"
        );
        assert!(
            reachable(),
            "recovered composer remains inside the paintable viewport"
        );
        input.focus().unwrap();
        render_shell(&frame).await;
        assert_physical_bottom(&dock, 852.0 - 26.0);
        assert!(reachable(), "uncontracted focused Send remains reachable");
        assert!(
            (frame_document
                .document_element()
                .unwrap()
                .get_bounding_client_rect()
                .height()
                - 852.0)
                .abs()
                <= 1.0,
            "focus must retain the recovered document paint surface"
        );
        let keyboard_readings = BrowserOverride(js_sys::Function::new_with_args("w", r#"
            const entries = [[w.screen, 'height', 932], [w.document.documentElement, 'clientHeight', 873],
                [w.visualViewport, 'height', 546], [w.visualViewport, 'offsetTop', 327],
                [w.visualViewport, 'pageTop', 386], [w, 'scrollY', 386]];
            const saved = entries.map(([object, key]) => Object.getOwnPropertyDescriptor(object, key));
            entries.forEach(([object, key, value]) => Object.defineProperty(object, key, {configurable:true, value}));
            const root = w.document.querySelector('.mobile-app');
            const transform = root.style.transform;
            root.style.transform = 'translateY(-327px)';
            w.dispatchEvent(new w.Event('resize'));
            return () => {
                root.style.transform = transform;
                entries.forEach(([object, key], i) => {
                    if (saved[i]) Object.defineProperty(object, key, saved[i]);
                    else delete object[key];
                });
                w.dispatchEvent(new w.Event('resize'));
            };
        "#).call1(&wasm_bindgen::JsValue::NULL, &frame.content_window().unwrap()).unwrap());
        render_shell(&frame).await;
        wasm_bindgen_test::console_log!(
            "Legacy keyboard shell top={}, composer bottom={}",
            shell.get_bounding_client_rect().top(),
            dock.get_bounding_client_rect().bottom()
        );
        assert!(
            shell.get_bounding_client_rect().top().abs() <= 1.0,
            "legacy keyboard pan must not shift the shell under the clock"
        );
        let header = container.query_selector(".chat-header").unwrap().unwrap();
        assert!(header.get_bounding_client_rect().top() >= 59.0);
        assert!(scroller.get_bounding_client_rect().top() >= 59.0);
        assert_physical_bottom(&dock, 546.0 - 10.0);
        assert!(
            reachable(),
            "keyboard-open visual Send center must hit Send"
        );
        assert_eq!(input.value(), "preserved draft");
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 0px; --tws-safe-bottom: 34px; }",
        ));
        js_sys::Function::new_with_args(
            "w",
            r#"
            Object.defineProperty(w.visualViewport, 'height', {configurable:true, value:487});
            Object.defineProperty(w.visualViewport, 'offsetTop', {configurable:true, value:386});
            w.document.querySelector('.mobile-app').style.transform = 'translateY(-386px)';
            w.dispatchEvent(new w.Event('resize'));
        "#,
        )
        .call1(
            &wasm_bindgen::JsValue::NULL,
            &frame.content_window().unwrap(),
        )
        .unwrap();
        render_shell(&frame).await;
        assert_physical_bottom(&dock, 487.0 - 10.0);
        assert!(reachable(), "fresh keyboard Send remains reachable");
        assert_eq!(input.value(), "preserved draft");
        drop(keyboard_readings);
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 59px; --tws-safe-bottom: 34px; }",
        ));
        input.blur().unwrap();
        render_shell(&frame).await;
        assert_physical_bottom(&dock, 852.0 - 26.0);
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 0px; --tws-safe-bottom: 34px; }",
        ));
        render_shell(&frame).await;
        assert_physical_bottom(&dock, 793.0 - 26.0);
        drop(readings);
        safe_area.set_text_content(Some(".mobile-app { --tws-safe-bottom: 0px; }"));
        render_shell(&frame).await;
        assert_bottom_gap(&dock, 8.0);
        // Linux WebKit loses the transcript end during resize. Disable Chrome's
        // automatic anchoring so this flow requires Tyde's scroll owner too.
        scroller
            .style()
            .set_property("overflow-anchor", "none")
            .unwrap();
        scroller.set_scroll_top(scroller.scroll_height());
        render_shell(&frame).await;
        for (width, height) in [(951, 585), (382, 580), (669, 867), (385, 571), (393, 852)] {
            frame
                .set_attribute(
                    "style",
                    &format!("width:{width}px;height:{height}px;border:0"),
                )
                .unwrap();
            render_shell(&frame).await;
            assert!(
                (scroller.scroll_height() - scroller.client_height() - scroller.scroll_top()).abs()
                    <= 1,
                "resizing a thread at its end must retain the latest message"
            );
        }
        let clearance =
            dock.get_bounding_client_rect().top() - transcript.get_bounding_client_rect().bottom();
        wasm_bindgen_test::console_log!("Transcript end clearance={clearance}");
        assert!(
            (clearance - 12.0).abs() <= 1.0,
            "exactly one measured end clearance, no empty typing spacer: {clearance}"
        );
        scroller.set_scroll_top(100);
        scroller
            .dispatch_event(&web_sys::Event::new("scroll").unwrap())
            .unwrap();
        state.push_chat_message_entry(&agent_ref, entry);
        render_shell(&frame).await;
        assert!(
            (scroller.scroll_top() - 100).abs() <= 1,
            "shared shell leaves history anchoring to Tyde"
        );
        input.focus().unwrap();
        input.set_selection_range(2, 8).unwrap();
        let original_input = input.clone();
        // Real iframe resizing replaces the retired controller's synthetic
        // --app-height writes. It proves layout, not native IME coverage.
        for (width, height) in [
            (393, 516),
            (852, 130),
            (852, 51),
            (320, 568),
            (430, 932),
            (932, 430),
            (382, 580),
            (669, 867),
            (951, 585),
            (1024, 768),
            (385, 571),
            (393, 852),
        ] {
            frame
                .set_attribute(
                    "style",
                    &format!("width:{width}px;height:{height}px;border:0"),
                )
                .unwrap();
            render_shell(&frame).await;
            let current = container
                .query_selector("[data-mobile-test='chat-input']")
                .unwrap()
                .unwrap();
            assert!(
                original_input.is_same_node(Some(&current)),
                "resize must retain the actual textarea"
            );
            assert_eq!(input.value(), "preserved draft");
            assert_eq!(input.selection_start().unwrap(), Some(2));
            assert_eq!(input.selection_end().unwrap(), Some(8));
            assert!(
                transcript.is_same_node(
                    container
                        .query_selector("[data-mobile-test='chat-transcript']")
                        .unwrap()
                        .as_ref()
                        .map(|element| element.as_ref())
                )
            );
            let header = container.query_selector(".chat-header").unwrap().unwrap();
            let header_bounds = header.get_bounding_client_rect();
            let dock_bounds = dock.get_bounding_client_rect();
            assert!(
                header_bounds.width() <= 840.0 && dock_bounds.width() <= 840.0,
                "wide chat chrome must stay on a readable rail"
            );
            assert!(
                (header_bounds.left() - dock_bounds.left()).abs() <= 1.0
                    && (header_bounds.right() - dock_bounds.right()).abs() <= 1.0,
                "header and composer must share the same rails"
            );
            for selector in [".chat-back-button", ".send-menu-toggle"] {
                let control = container.query_selector(selector).unwrap().unwrap();
                let bounds = control.get_bounding_client_rect();
                assert!(
                    bounds.width() >= 44.0 && bounds.height() >= 44.0,
                    "{selector} must remain a full touch target"
                );
            }
            wasm_bindgen_test::console_log!(
                "Shell resize {width}x{height}: send_top={} send_bottom={} shell={} viewport={} document_h={}",
                send.get_bounding_client_rect().top(),
                send.get_bounding_client_rect().bottom(),
                container
                    .query_selector(".mobile-app")
                    .unwrap()
                    .unwrap()
                    .get_attribute("style")
                    .unwrap_or_default(),
                frame
                    .content_window()
                    .unwrap()
                    .visual_viewport()
                    .unwrap()
                    .height(),
                frame_document.document_element().unwrap().client_height()
            );
            let send_bounds = send.get_bounding_client_rect();
            let hit = frame_document.element_from_point(
                (send_bounds.x() + send_bounds.width() / 2.0) as f32,
                (send_bounds.y() + send_bounds.height() / 2.0) as f32,
            );
            wasm_bindgen_test::console_log!(
                "Composer hit test: send_left={} send_width={} hit_tag={:?} hit_class={:?} frame_hidden={}",
                send_bounds.left(),
                send_bounds.width(),
                hit.as_ref().map(web_sys::Element::tag_name),
                hit.as_ref().map(web_sys::Element::class_name),
                frame_document.hidden()
            );
            assert!(
                reachable(),
                "composer hit target is reachable at {width}x{height}"
            );
            assert_bottom_gap(&dock, if height < 181 { 2.0 } else { 8.0 });
            assert!(send.get_bounding_client_rect().bottom() <= f64::from(height));
            assert!(
                frame_document
                    .active_element()
                    .unwrap()
                    .is_same_node(Some(&input))
            );
        }
        // Background ends owned editing; return keeps the draft and selection
        // and waits for a tap instead of refocusing.
        assert!(!frame_document.hidden());
        let set_hidden = js_sys::Function::new_with_args(
            "w,hidden",
            r#"
            for (const key of ['hidden', 'visibilityState']) delete w.document[key];
            if (hidden) {
                Object.defineProperty(w.document, 'hidden', {configurable:true, value:true});
                Object.defineProperty(w.document, 'visibilityState', {configurable:true, value:'hidden'});
            }
            w.document.dispatchEvent(new w.Event('visibilitychange'));
        "#,
        );
        let frame_window = frame.content_window().unwrap();
        for hidden in [true, false] {
            set_hidden
                .call2(&wasm_bindgen::JsValue::NULL, &frame_window, &hidden.into())
                .unwrap();
            render_shell(&frame).await;
            assert_eq!(frame_document.hidden(), hidden);
            assert!(
                !frame_document
                    .active_element()
                    .is_some_and(|active| active.is_same_node(Some(&input))),
                "hidden={hidden}: background must end composer editing without refocus"
            );
            assert_eq!(input.value(), "preserved draft");
            assert_eq!(state.chat_input.get_untracked(), "preserved draft");
            assert_eq!(input.selection_start().unwrap(), Some(2));
            assert_eq!(input.selection_end().unwrap(), Some(8));
        }
        // iOS can report a stale contracted viewport with no keyboard after a
        // foreign-keyboard round trip; unfocused, it must not strand the shell.
        let contract_viewport = js_sys::Function::new_with_args(
            "w",
            r#"
            const saved = Object.getOwnPropertyDescriptor(w.visualViewport, 'height');
            Object.defineProperty(w.visualViewport, 'height', {configurable:true, value:492});
            w.visualViewport.dispatchEvent(new w.Event('resize'));
            return () => {
                if (saved) Object.defineProperty(w.visualViewport, 'height', saved);
                else delete w.visualViewport.height;
                w.visualViewport.dispatchEvent(new w.Event('resize'));
            };
        "#,
        );
        let stale_contraction = BrowserOverride(
            contract_viewport
                .call1(&wasm_bindgen::JsValue::NULL, &frame_window)
                .unwrap(),
        );
        render_shell(&frame).await;
        assert_eq!(frame_window.visual_viewport().unwrap().height(), 492.0);
        assert!(
            (shell.get_bounding_client_rect().height() - 852.0).abs() <= 1.0,
            "unfocused stale contraction must keep the full shell: {}",
            shell.get_bounding_client_rect().height()
        );
        assert!(!shell.has_attribute("data-tws-keyboard"));
        drop(stale_contraction);
        render_shell(&frame).await;
        assert_bottom_gap(&dock, 8.0);
        assert!(reachable(), "returned composer remains reachable");
        input.focus().unwrap();
        assert_eq!(input.selection_start().unwrap(), Some(2));
        assert_eq!(input.selection_end().unwrap(), Some(8));
        // The old gate rejected a 307.19 ms sample despite correct Send hit
        // geometry. Assert before and after the reactive flush, not host speed.
        let set_input = js_sys::Function::new_with_args(
            "input,value",
            r#"
            input.value = value;
            input.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText'}));
        "#,
        );
        let contact = js_sys::Function::new_with_args(
            "button,type,x,y",
            r#"
            button.dispatchEvent(new PointerEvent(type, {
                bubbles:true, pointerType:'touch', clientX:x, clientY:y, pointerId:1
            }));
        "#,
        );
        let center = || {
            let r = send.get_bounding_client_rect();
            (r.x() + r.width() / 2.0, r.y() + r.height() / 2.0)
        };
        let hit_at = |point: (f64, f64)| {
            frame_document
                .element_from_point(point.0 as f32, point.1 as f32)
                .unwrap()
        };
        let down_or_up = |kind: &str, point: (f64, f64)| {
            contact
                .call4(
                    &wasm_bindgen::JsValue::NULL,
                    &send,
                    &kind.into(),
                    &point.0.into(),
                    &point.1.into(),
                )
                .unwrap();
        };
        let contact_guard = crate::bridge::test_defer_sends();
        for resize_during_contact in [false, true] {
            for (shape, draft) in [
                ("wrap", "a wrapped composing draft ".repeat(10)),
                ("shrink", "short".to_owned()),
                ("newline", "one\ntwo\nthree\nfour\nfive\nsix".to_owned()),
                ("shrink", "short".to_owned()),
                ("maxheight", "line\n".repeat(40)),
                ("shrink", "short".to_owned()),
            ] {
                let attempts_before = crate::bridge::test_send_attempts();
                let original_height = input.get_bounding_client_rect().height();
                let original_point = center();
                if resize_during_contact {
                    down_or_up("pointerdown", original_point);
                }
                set_input
                    .call2(&wasm_bindgen::JsValue::NULL, &input, &draft.clone().into())
                    .unwrap();
                let point = if resize_during_contact {
                    original_point
                } else {
                    center()
                };
                if !resize_during_contact {
                    down_or_up("pointerdown", point);
                }
                assert!(
                    reachable(),
                    "Send must be reachable synchronously during resize"
                );
                assert!(
                    send.contains(Some(&hit_at(point))),
                    "Contact must remain on Send before the reactive flush"
                );
                assert!(input.value() == draft && state.chat_input.get_untracked() == draft);
                next_tick().await;
                let height = input.get_bounding_client_rect().height();
                if shape == "shrink" {
                    assert!(height < original_height, "short draft must shrink");
                } else {
                    assert!(height > original_height, "multiline draft must grow");
                }
                if shape == "maxheight" {
                    let max = frame
                        .content_window()
                        .unwrap()
                        .get_computed_style(&input)
                        .unwrap()
                        .unwrap()
                        .get_property_value("max-height")
                        .unwrap()
                        .trim_end_matches("px")
                        .parse::<f64>()
                        .unwrap();
                    assert!(
                        (height - max).abs() <= 1.0,
                        "long draft reaches the CSS cap"
                    );
                    assert!(
                        f64::from(input.scroll_height()) > height,
                        "capped draft scrolls internally"
                    );
                }
                assert!(send.is_connected());
                assert!(
                    send.is_same_node(
                        container
                            .query_selector("[data-mobile-test='chat-send']")
                            .unwrap()
                            .as_ref()
                            .map(|element| element.as_ref())
                    )
                );
                assert!(!send.has_attribute("disabled"));
                assert!(
                    reachable(),
                    "current Send center remains reachable during resize"
                );
                assert!(
                    send.contains(Some(&hit_at(point))),
                    "original contact stays on Send during resize"
                );
                assert!(input.value() == draft && state.chat_input.get_untracked() == draft);
                down_or_up("pointerup", point);
                send.unchecked_ref::<HtmlElement>().click();
                next_tick().await;
                assert_eq!(crate::bridge::test_send_attempts(), attempts_before + 1);
                let lines = crate::bridge::test_sent_lines();
                assert_eq!(lines.len(), attempts_before + 1);
                let sent: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
                // Send now carries one typed cancel-and-deliver operation; the
                // original resized draft must survive inside that operation.
                assert_eq!(sent["kind"], "human_input");
                let submission: protocol::HumanInputPayload =
                    serde_json::from_value(sent["payload"].clone()).unwrap();
                assert_eq!(
                    submission.action,
                    protocol::HumanInputAction::InterruptAndSend
                );
                assert!(!submission.id.0.is_empty());
                assert!(
                    submission.input.message == draft.trim(),
                    "resized draft reaches local admission"
                );
                crate::bridge::test_resolve_next_send();
                next_tick().await;
                next_tick().await;
                assert!(input.value().is_empty());
                // Restore the shape for the next grow/shrink contact.
                set_input
                    .call2(&wasm_bindgen::JsValue::NULL, &input, &draft.into())
                    .unwrap();
                next_tick().await;
            }
        }
        set_input
            .call2(
                &wasm_bindgen::JsValue::NULL,
                &input,
                &"preserved draft".into(),
            )
            .unwrap();
        next_tick().await;
        crate::dispatch::reset_inbound_seq_for_host(&agent_ref.local_host_id);
        for (seq, activity) in [
            protocol::AgentActivity::Thinking,
            protocol::AgentActivity::Idle,
        ]
        .into_iter()
        .enumerate()
        {
            let attempts_before = crate::bridge::test_send_attempts();
            let point = center();
            down_or_up("pointerdown", point);
            crate::dispatch::dispatch_envelope(
                &state,
                &agent_ref.local_host_id,
                protocol::Envelope::from_payload(
                    protocol::StreamPath("/agent/composer-agent/instance".to_owned()),
                    protocol::FrameKind::AgentActivityChanged,
                    seq as u64,
                    &protocol::AgentActivityChangedPayload { activity },
                )
                .unwrap(),
            );
            crate::dispatch::apply_chat_event(
                &state,
                &agent_ref,
                protocol::ChatEvent::StreamStart(protocol::StreamStartData {
                    agent: "fixture".to_owned(),
                    model: None,
                }),
            );
            crate::dispatch::apply_chat_event(
                &state,
                &agent_ref,
                protocol::ChatEvent::StreamDelta(protocol::StreamTextDeltaData {
                    text: "streamed output".to_owned(),
                }),
            );
            next_tick().await;
            assert!(
                container
                    .query_selector("[data-mobile-test='chat-streaming']")
                    .unwrap()
                    .unwrap()
                    .text_content()
                    .unwrap()
                    .contains("streamed output")
            );
            assert!(send.is_connected());
            assert!(
                send.is_same_node(
                    container
                        .query_selector("[data-mobile-test='chat-send']")
                        .unwrap()
                        .as_ref()
                        .map(|element| element.as_ref())
                )
            );
            assert_eq!(send.text_content().unwrap().trim(), "Send");
            assert!(!send.has_attribute("disabled"));
            assert!(reachable() && send.contains(Some(&hit_at(point))));
            let ring = container
                .query_selector("[data-mobile-test='chat-thinking-ring']")
                .unwrap();
            assert_eq!(
                ring.is_some(),
                activity == protocol::AgentActivity::Thinking
            );
            if let Some(ring) = ring {
                let style = frame
                    .content_window()
                    .unwrap()
                    .get_computed_style(&ring)
                    .unwrap()
                    .unwrap();
                assert_eq!(style.get_property_value("pointer-events").unwrap(), "none");
            }
            down_or_up("pointerup", point);
            send.unchecked_ref::<HtmlElement>().click();
            next_tick().await;
            assert_eq!(crate::bridge::test_send_attempts(), attempts_before + 1);
            let lines = crate::bridge::test_sent_lines();
            assert_eq!(lines.len(), attempts_before + 1);
            let sent: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
            assert_eq!(sent["kind"], "human_input");
            let submission: protocol::HumanInputPayload =
                serde_json::from_value(sent["payload"].clone()).unwrap();
            assert_eq!(
                submission.action,
                protocol::HumanInputAction::InterruptAndSend,
                "lifecycle must not change the single cancel-and-deliver operation"
            );
            assert!(!submission.id.0.is_empty());
            assert_eq!(submission.input.message, "preserved draft");
            crate::bridge::test_resolve_next_send();
            next_tick().await;
            next_tick().await;
            set_input
                .call2(
                    &wasm_bindgen::JsValue::NULL,
                    &input,
                    &"preserved draft".into(),
                )
                .unwrap();
            next_tick().await;
            wasm_bindgen_test::console_log!(
                "LIFECYCLE_CONTACT activity={:?} node_same=true center_hit=true original_hit=true human_input=1",
                activity
            );
        }
        let attempts_before_overlays = crate::bridge::test_send_attempts();
        // A menu intentionally catches the first outside tap. Closing it must remove interception.
        let toggle = container
            .query_selector("[data-mobile-test='chat-send-menu-toggle']")
            .unwrap()
            .unwrap();
        for iteration in 0..2 {
            let point = center();
            toggle.unchecked_ref::<HtmlElement>().click();
            next_tick().await;
            for (edge, activity) in [
                protocol::AgentActivity::Thinking,
                protocol::AgentActivity::Idle,
            ]
            .into_iter()
            .enumerate()
            {
                crate::dispatch::dispatch_envelope(
                    &state,
                    &agent_ref.local_host_id,
                    protocol::Envelope::from_payload(
                        protocol::StreamPath("/agent/composer-agent/instance".to_owned()),
                        protocol::FrameKind::AgentActivityChanged,
                        2 + iteration * 2 + edge as u64,
                        &protocol::AgentActivityChangedPayload { activity },
                    )
                    .unwrap(),
                );
                next_tick().await;
                assert_eq!(
                    container
                        .query_selector("[data-mobile-test='chat-send-menu-steer']")
                        .unwrap()
                        .is_some(),
                    activity == protocol::AgentActivity::Thinking,
                    "Steer is a lifecycle-dependent menu item, not the stable primary"
                );
                assert!(send.is_connected());
                assert!(
                    send.is_same_node(Some(
                        &container
                            .query_selector("[data-mobile-test='chat-send']")
                            .unwrap()
                            .unwrap()
                    ))
                );
            }
            let backdrop = container
                .query_selector("[data-mobile-test='chat-send-menu-backdrop']")
                .unwrap()
                .unwrap();
            assert!(
                backdrop.is_same_node(Some(&hit_at(point))),
                "open menu deliberately intercepts original Send contact"
            );
            assert!(backdrop.is_same_node(Some(&hit_at(center()))));
            hit_at(point).unchecked_ref::<HtmlElement>().click();
            next_tick().await;
            assert!(!backdrop.is_connected());
            assert!(
                reachable() && send.contains(Some(&hit_at(point))),
                "closed menu must leave no stale interceptor"
            );
        }
        crate::dispatch::apply_chat_event(
            &state,
            &agent_ref,
            protocol::ChatEvent::ToolProgress(protocol::ToolProgressData {
                tool_call_id: "contact-background".to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::Other {
                    payload: serde_json::json!({"description":"Background task"}),
                },
            }),
        );
        next_tick().await;
        let drawer_toggle = container
            .query_selector("[data-mobile-test='activity-toggle']")
            .unwrap()
            .unwrap();
        let point = center();
        for open in [true, false] {
            drawer_toggle.unchecked_ref::<HtmlElement>().click();
            next_tick().await;
            assert_eq!(
                drawer_toggle.get_attribute("aria-expanded").as_deref(),
                Some(if open { "true" } else { "false" })
            );
            assert!(
                reachable() && send.contains(Some(&hit_at(point))),
                "drawer transition must not intercept Send"
            );
            render_shell(&frame).await;
            let settled = js_sys::Function::new_with_args(
                "document",
                r#"
                return Promise.all(document.getAnimations()
                    .filter(animation => animation.effect.getComputedTiming().iterations !== Infinity)
                    .map(animation => animation.finished));
            "#,
            )
            .call1(&wasm_bindgen::JsValue::NULL, &frame_document)
            .unwrap();
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(settled))
                .await
                .unwrap();
            render_shell(&frame).await;
            assert!(
                reachable() && send.contains(Some(&hit_at(point))),
                "settled drawer must not intercept Send"
            );
        }
        wasm_bindgen_test::console_log!(
            "OVERLAY_CONTACT menu_open=intercept menu_closed=reachable drawer_open_closed=reachable"
        );
        assert_eq!(
            crate::bridge::test_send_attempts(),
            attempts_before_overlays,
            "overlay disclosure and dismissal do not submit"
        );
        drop(contact_guard);
        input.blur().unwrap();
        state.active_agent.set(None);
        render_shell(&frame).await;
        let options = container
            .query_selector("[data-mobile-test='new-chat-options']")
            .unwrap()
            .unwrap();
        let intro = container
            .query_selector("[data-mobile-test='chat-empty-new']")
            .unwrap()
            .unwrap();
        assert_eq!(scroller.scroll_top(), 0, "new-chat setup starts at the top");
        // Air measured landscape safe insets of 47px/62px. Supply the CSS
        // inputs, not a claim that an iframe reproduces native keyboards.
        for (width, height, safe) in [
            (320, 369, 0),
            (440, 956, 0),
            (844, 142, 47),
            (844, 390, 47),
            (956, 86, 62),
            (956, 82, 62),
            (956, 330, 62),
            (440, 956, 0),
        ] {
            safe_area.set_text_content(Some(&format!(
                ".mobile-app {{ --tws-safe-bottom: 0px; --tws-safe-left: {safe}px; --tws-safe-right: {safe}px; }}"
            )));
            frame
                .set_attribute(
                    "style",
                    &format!("width:{width}px;height:{height}px;border:0"),
                )
                .unwrap();
            render_shell(&frame).await;
            assert!(
                options.get_bounding_client_rect().bottom()
                    <= intro.get_bounding_client_rect().top(),
                "new-chat setup and introduction share non-overlapping flow"
            );
            for element in [&options, &intro, &dock] {
                let rect = element.get_bounding_client_rect();
                assert!(
                    rect.left() >= f64::from(safe) && rect.right() <= f64::from(width - safe),
                    "new-chat content stays inside horizontal safe insets at {width}x{height}"
                );
            }
            assert!(reachable(), "new-chat Send stays reachable after resize");
            assert!(
                input.is_same_node(
                    container
                        .query_selector("[data-mobile-test='chat-input']")
                        .unwrap()
                        .as_ref()
                        .map(|element| element.as_ref())
                ),
                "new-chat setup must not replace the composer"
            );
            let input_bounds = input.get_bounding_client_rect();
            let send_bounds = send.get_bounding_client_rect();
            assert!(input_bounds.height() >= 44.0);
            assert!(
                input_bounds.top() >= 0.0 && input_bounds.bottom() <= f64::from(height),
                "the whole new-chat input fits the usable viewport"
            );
            assert!(
                send_bounds.top() >= 0.0 && send_bounds.bottom() <= f64::from(height),
                "the whole new-chat Send fits the usable viewport"
            );
            for selector in [
                "new-chat-project",
                "new-chat-backend",
                "new-chat-agent",
                "new-chat-session-settings",
            ] {
                let control = container
                    .query_selector(&format!("[data-mobile-test='{selector}']"))
                    .unwrap()
                    .unwrap();
                control.scroll_into_view();
                render_shell(&frame).await;
                let rect = control.get_bounding_client_rect();
                assert!(
                    rect.left() >= f64::from(safe) && rect.right() <= f64::from(width - safe),
                    "new-chat {selector} stays inside horizontal safe insets"
                );
                assert!(
                    frame_document
                        .element_from_point(
                            (rect.x() + rect.width() / 2.0) as f32,
                            (rect.y() + rect.height() / 2.0) as f32,
                        )
                        .is_some_and(|hit| control.contains(Some(&hit))),
                    "new-chat {selector} is scroll-reachable at {width}x{height}"
                );
            }
            intro.scroll_into_view();
            render_shell(&frame).await;
            let rect = intro.get_bounding_client_rect();
            assert!(
                rect.top() < dock.get_bounding_client_rect().top()
                    && rect.bottom() > scroller.get_bounding_client_rect().top(),
                "introduction remains scroll-reachable"
            );
        }
        frame
            .set_attribute("style", "width:393px;height:852px;border:0")
            .unwrap();
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: agent_ref.local_host_id.clone(),
            agent_id: agent_ref.agent_id.clone(),
        }));
        render_shell(&frame).await;
        assert!(!options.is_connected() && !intro.is_connected());
        state.viewing_chat.set(false);
        state.active_tab.set(MobileTab::Home);
        render_shell(&frame).await;
        wasm_bindgen_test::console_log!(
            "Dashboard stale-focus fixture: loading={} stat_present={}",
            container
                .query_selector("[data-mobile-test='home-loading']")
                .unwrap()
                .is_some(),
            container
                .query_selector("[data-mobile-test='home-stat-agents']")
                .unwrap()
                .is_some()
        );
        let stale_contraction = BrowserOverride(
            contract_viewport
                .call1(&wasm_bindgen::JsValue::NULL, &frame_window)
                .unwrap(),
        );
        let stat = container
            .query_selector("[data-mobile-test='home-stat-agents']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert!(stat.text_content().unwrap().contains("Active agents"));
        stat.focus().unwrap();
        render_shell(&frame).await;
        assert!(
            !frame_document
                .active_element()
                .is_some_and(|active| active.is_same_node(Some(&stat))),
            "a non-interactive card must not acquire input ownership"
        );
        assert!(
            (shell.get_bounding_client_rect().height() - 852.0).abs() <= 1.0,
            "non-interactive cards must not reclaim a stale contraction"
        );
        assert!(!shell.has_attribute("data-tws-keyboard"));
        let nav = container
            .query_selector("[data-mobile-test='bottom-nav']")
            .unwrap()
            .unwrap();
        assert!(nav.get_bounding_client_rect().height() >= 44.0);
        drop(stale_contraction);
        let readings = standalone_readings();
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 59px; --tws-safe-bottom: 34px; }",
        ));
        for tab in [
            MobileTab::Home,
            MobileTab::Agents,
            MobileTab::Sessions,
            MobileTab::Settings,
        ] {
            state.active_tab.set(tab);
            render_shell(&frame).await;
            let nav = container
                .query_selector("[data-mobile-test='bottom-nav']")
                .unwrap()
                .unwrap();
            let bounds = nav.get_bounding_client_rect();
            assert!(bounds.height() >= 44.0);
            assert_bottom_gap(&nav, 26.0);
            assert_physical_bottom(&nav, 852.0 - 26.0);
            let pane = container.query_selector(".tws-content").unwrap().unwrap();
            assert!(pane.get_bounding_client_rect().top() >= 59.0);
            assert!(
                frame_document
                    .element_from_point(
                        (bounds.x() + bounds.width() / 2.0) as f32,
                        (bounds.bottom() - 6.0) as f32
                    )
                    .is_some_and(|hit| nav.contains(Some(&hit)))
            );
            safe_area.set_text_content(Some(
                ".mobile-app { --tws-safe-top: 0px; --tws-safe-bottom: 34px; }",
            ));
            render_shell(&frame).await;
            assert_physical_bottom(&nav, 793.0 - 26.0);
            safe_area.set_text_content(Some(
                ".mobile-app { --tws-safe-top: 59px; --tws-safe-bottom: 34px; }",
            ));
        }
        drop(readings);
        safe_area.set_text_content(Some(".mobile-app { --tws-safe-bottom: 0px; }"));
        let theme_control = container
            .query_selector("[data-mobile-test='settings-theme']")
            .unwrap()
            .unwrap();
        for (width, height) in [(669, 867), (1024, 768), (385, 571), (669, 867)] {
            frame
                .set_attribute(
                    "style",
                    &format!("width:{width}px;height:{height}px;border:0"),
                )
                .unwrap();
            render_shell(&frame).await;
            assert!(
                theme_control.is_same_node(
                    container
                        .query_selector("[data-mobile-test='settings-theme']")
                        .unwrap()
                        .as_ref()
                        .map(|element| element.as_ref())
                )
            );
            let sections = container.query_selector_all(".settings-section").unwrap();
            let first: web_sys::Element = sections.item(0).unwrap().unchecked_into();
            let second: web_sys::Element = sections.item(1).unwrap().unchecked_into();
            let first = first.get_bounding_client_rect();
            let second = second.get_bounding_client_rect();
            if width >= 640 {
                assert!(
                    (first.top() - second.top()).abs() <= 1.0 && second.left() > first.right(),
                    "wide settings must use two readable, non-overlapping columns"
                );
            } else {
                assert!(
                    second.top() >= first.bottom(),
                    "Split View must restore the single-column settings list"
                );
            }
            let nav = container
                .query_selector("[data-mobile-test='bottom-nav']")
                .unwrap()
                .unwrap();
            assert!(
                nav.get_bounding_client_rect().width() <= 480.0,
                "wide navigation stays a compact dock"
            );
        }
        frame
            .set_attribute("style", "width:594px;height:368px;border:0")
            .unwrap();
        safe_area.set_text_content(Some(
            ".mobile-app { --tws-safe-top: 20px; --tws-safe-bottom: 21px; --tws-safe-left: 62px; --tws-safe-right: 47px; }",
        ));
        let sheet_viewport = BrowserOverride(
            js_sys::Function::new_with_args(
                "w",
                r#"
                const viewport = w.visualViewport;
                const saved = Object.getOwnPropertyDescriptor(viewport, 'height');
                Object.defineProperty(viewport, 'height', {configurable:true, value:180});
                viewport.dispatchEvent(new w.Event('resize'));
                return () => {
                    if (saved) Object.defineProperty(viewport, 'height', saved);
                    else delete viewport.height;
                    viewport.dispatchEvent(new w.Event('resize'));
                };
            "#,
            )
            .call1(
                &wasm_bindgen::JsValue::NULL,
                &frame.content_window().unwrap(),
            )
            .unwrap(),
        );
        for session_settings in [false, true] {
            state.project_picker_open.set(!session_settings);
            state.session_settings_open.set(session_settings);
            render_shell(&frame).await;
            let selector = if session_settings {
                ".session-settings-sheet"
            } else {
                ".project-sheet"
            };
            let sheet = container.query_selector(selector).unwrap().unwrap();
            let bounds = sheet.get_bounding_client_rect();
            let shell_bounds = shell.get_bounding_client_rect();
            assert!((shell_bounds.height() - 180.0).abs() <= 1.0);
            assert!(
                bounds.left() >= 62.0
                    && bounds.right() <= 594.0 - 47.0
                    && bounds.top() >= shell_bounds.top() + 20.0
                    && bounds.bottom() <= shell_bounds.bottom() - 21.0,
                "landscape sheets must respect the shell's contracted viewport and safe edges"
            );
        }
        drop(sheet_viewport);
        state.session_settings_open.set(false);
        safe_area.set_text_content(Some(".mobile-app { --tws-safe-bottom: 0px; }"));
        frame
            .set_attribute("style", "width:852px;height:130px;border:0")
            .unwrap();
        render_shell(&frame).await;
        let nav = container
            .query_selector("[data-mobile-test='bottom-nav']")
            .unwrap()
            .unwrap();
        assert_eq!(
            nav.get_bounding_client_rect().height(),
            0.0,
            "compact shell covers tabs rather than competing with content"
        );
        frame
            .set_attribute("style", "width:393px;height:852px;border:0")
            .unwrap();
        render_shell(&frame).await;
        assert!(
            nav.get_bounding_client_rect().height() >= 44.0,
            "tabs return after expansion"
        );
        assert_bottom_gap(&nav, 8.0);

        fn assert_manipulation_gesture_policy(
            document: &web_sys::Document,
            required_selectors: &[&str],
        ) {
            let window = document.default_view().unwrap();
            for root in [
                document.document_element().unwrap(),
                document.body().unwrap().into(),
            ] {
                let style = window.get_computed_style(&root).unwrap().unwrap();
                assert_eq!(
                    style.get_property_value("touch-action").unwrap(),
                    "manipulation",
                    "document root surface {} must disable double-tap zoom via touch-action: manipulation",
                    root.tag_name()
                );
            }
            for selector in required_selectors {
                let element = document
                    .query_selector(selector)
                    .unwrap()
                    .unwrap_or_else(|| panic!("expected mounted mobile surface {selector}"));
                let style = window.get_computed_style(&element).unwrap().unwrap();
                assert_eq!(
                    style.get_property_value("touch-action").unwrap(),
                    "manipulation",
                    "mobile surface {selector} must use touch-action: manipulation"
                );
            }
            let app_root = document
                .query_selector(".mobile-app")
                .unwrap()
                .expect("expected mounted .mobile-app root")
                .unchecked_into::<HtmlElement>();
            assert!(
                app_root.onclick().is_some(),
                ".mobile-app root must install a direct onclick listener so WebKit iOS potentialTapAtPosition resolves a click-responding ancestor with touch-action: manipulation inside WKChildScrollView scrollers"
            );
            let all = document
                .query_selector_all(".mobile-app, .mobile-app *")
                .unwrap();
            assert!(all.length() > 0, "expected mounted .mobile-app subtree");
            for index in 0..all.length() {
                let element = all
                    .item(index)
                    .unwrap()
                    .unchecked_into::<web_sys::Element>();
                let style = window.get_computed_style(&element).unwrap().unwrap();
                assert_eq!(
                    style.get_property_value("touch-action").unwrap(),
                    "manipulation",
                    "every rendered mobile element and overflow scroller must keep touch-action: manipulation (tag={}, class={})",
                    element.tag_name(),
                    element.class_name()
                );
            }
        }

        async fn focus_readable_control(
            document: &web_sys::Document,
            selector: &str,
            minimum_font: f64,
        ) -> HtmlElement {
            let control = document
                .query_selector(selector)
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap();
            let window = document.default_view().unwrap();
            let style = window.get_computed_style(&control).unwrap().unwrap();
            let font = style.get_property_value("font-size").unwrap();
            assert!(
                font.trim_end_matches("px").parse::<f64>().unwrap() >= minimum_font,
                "native editing text must not trigger iOS focus magnification: {selector}, {font}"
            );
            assert_eq!(
                style.get_property_value("touch-action").unwrap(),
                "manipulation",
                "editable control must also suppress double-tap page zoom: {selector}"
            );
            control.focus().unwrap();
            next_tick().await;
            window
                .dispatch_event(&web_sys::Event::new("pageshow").unwrap())
                .unwrap();
            next_tick().await;
            assert!(
                document
                    .active_element()
                    .unwrap()
                    .is_same_node(Some(&control)),
                "page restoration must retain the same focused control"
            );
            assert!(control.get_bounding_client_rect().height() > 0.0);
            control
        }

        state.active_tab.set(MobileTab::Sessions);
        next_tick().await;
        assert_manipulation_gesture_policy(
            &frame_document,
            &[
                ".mobile-app",
                ".tws-content",
                ".tws-bottom",
                "[data-mobile-test='bottom-nav']",
                "[data-mobile-test='nav-tab-home']",
                "[data-mobile-test='nav-tab-agents']",
                "[data-mobile-test='nav-tab-sessions']",
                "[data-mobile-test='nav-tab-settings']",
                "[data-mobile-test='sessions-view']",
            ],
        );

        state.viewing_chat.set(true);
        next_tick().await;
        assert_manipulation_gesture_policy(
            &frame_document,
            &[
                ".mobile-app",
                ".chat-header",
                "[data-mobile-test='chat-back']",
                "[data-mobile-test='chat-more']",
                "[data-mobile-test='chat-messages']",
                "[data-mobile-test='chat-transcript']",
                ".chat-message",
                "[data-mobile-test='chat-bottom-dock']",
                "[data-mobile-test='chat-input-container']",
                "[data-mobile-test='chat-input']",
                "[data-mobile-test='chat-send']",
                "[data-mobile-test='chat-send-menu-toggle']",
            ],
        );
        let live_scroller = frame_document
            .query_selector("[data-mobile-test='chat-messages']")
            .unwrap()
            .unwrap();
        let scroller_style = frame
            .content_window()
            .unwrap()
            .get_computed_style(&live_scroller)
            .unwrap()
            .unwrap();
        assert_eq!(
            scroller_style.get_property_value("overflow-y").unwrap(),
            "auto",
            "chat scroller must preserve ordinary vertical scrolling"
        );
        let live_transcript = frame_document
            .query_selector("[data-mobile-test='chat-transcript']")
            .unwrap()
            .unwrap();
        let transcript_style = frame
            .content_window()
            .unwrap()
            .get_computed_style(&live_transcript)
            .unwrap()
            .unwrap();
        assert_ne!(
            transcript_style.get_property_value("user-select").unwrap(),
            "none",
            "chat transcript must preserve text selection"
        );
        focus_readable_control(&frame_document, "[data-mobile-test='chat-input']", 16.0).await;
        for selector in [
            "[data-mobile-test='chat-more']",
            "[data-mobile-test='chat-menu-rename']",
        ] {
            container
                .query_selector(selector)
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
                .click();
            next_tick().await;
        }
        focus_readable_control(
            &frame_document,
            "[data-mobile-test='chat-rename-input']",
            16.0,
        )
        .await;
        container
            .query_selector("[data-mobile-test='chat-rename-cancel']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        state.viewing_chat.set(false);
        state.active_tab.set(MobileTab::Sessions);
        next_tick().await;
        let search = focus_readable_control(
            &frame_document,
            "[data-mobile-test='sessions-search']",
            16.0,
        )
        .await;
        let search = search.dyn_into::<web_sys::HtmlInputElement>().unwrap();
        search.set_value("No matching session");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        focus_readable_control(
            &frame_document,
            "[data-mobile-test='sessions-search']",
            16.0,
        )
        .await;
        assert!(
            container
                .query_selector("[data-mobile-test='sessions-empty-search']")
                .unwrap()
                .is_some()
        );
        assert!(search.value() == "No matching session");
        state.active_tab.set(MobileTab::Settings);
        next_tick().await;
        for selector in [
            "[data-mobile-test='settings-theme']",
            "[data-mobile-test='settings-tool-output']",
        ] {
            focus_readable_control(&frame_document, selector, 16.0).await;
        }
        state
            .app_mode
            .set(AppMode::Pairing(PairingScreen::ManualPaste));
        next_tick().await;
        focus_readable_control(&frame_document, ".pairing-paste-input", 16.0).await;
        frame_document
            .document_element()
            .unwrap()
            .unchecked_ref::<HtmlElement>()
            .style()
            .set_property("font-size", "20px")
            .unwrap();
        focus_readable_control(&frame_document, ".pairing-paste-input", 20.0).await;
        let app_root = container
            .query_selector(".mobile-app")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        app_root.style().set_property("font-size", "24px").unwrap();
        focus_readable_control(&frame_document, ".pairing-paste-input", 24.0).await;
        drop(mount);
        assert!(
            app_root.onclick().is_none(),
            "unmount clears .mobile-app direct onclick listener"
        );
        assert!(
            !frame_document
                .document_element()
                .unwrap()
                .has_attribute("data-tws-document"),
            "unmount releases document ownership"
        );
        frame.remove();
        let platform_override = BrowserOverride(js_sys::Function::new_no_args(r#"
            const p=Object.getOwnPropertyDescriptor(navigator,'platform');
            const s=Object.getOwnPropertyDescriptor(navigator,'standalone');
            Object.defineProperty(navigator,'platform',{configurable:true,value:'iPhone'});
            Object.defineProperty(navigator,'standalone',{configurable:true,value:true});
            return () => {
                if(p)Object.defineProperty(navigator,'platform',p);else delete navigator.platform;
                if(s)Object.defineProperty(navigator,'standalone',s);else delete navigator.standalone;
            };
        "#).call0(&wasm_bindgen::JsValue::NULL).unwrap());
        let window = web_sys::window().unwrap();
        let history = window.history().unwrap();
        let original_url = window.location().href().unwrap();
        let legacy_url = web_sys::Url::new(&original_url).unwrap();
        legacy_url.search_params().set("tyde-send-diagnostics", "1");
        history
            .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&legacy_url.href()))
            .unwrap();
        let privacy_container = make_container();
        let privacy_state = AppState::new();
        let host = LocalHostId("privacy-host".to_owned());
        privacy_state.active_local_host_id.set(Some(host.clone()));
        privacy_state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), ConnectionStatus::Connected);
        });
        let mount_state = privacy_state.clone();
        let privacy_mount = mount_to(privacy_container.clone(), move || {
            provide_context(mount_state);
            view! { <AppSurface /> }
        });
        history
            .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&original_url))
            .unwrap();
        let assert_no_capture = || {
            assert!(
                privacy_container
                    .query_selector("[data-mobile-test='send-diagnostics-status'], [data-mobile-test='send-diagnostics-panel']")
                    .unwrap()
                    .is_none(),
                "device diagnostics must have no app or Settings surface"
            );
            assert_eq!(
                js_sys::eval("typeof window.__TYDE_SEND_DIAGNOSTICS__")
                    .unwrap()
                    .as_string()
                    .as_deref(),
                Some("undefined"),
                "iOS standalone must not create a recorder, even with the legacy opt-in"
            );
        };
        for mode in [
            AppMode::Onboarding,
            AppMode::Pairing(crate::state::PairingScreen::ManualPaste),
            AppMode::Workspace,
        ] {
            privacy_state.app_mode.set(mode);
            next_tick().await;
            assert_no_capture();
        }
        privacy_state.active_tab.set(MobileTab::Settings);
        next_tick().await;
        assert_no_capture();
        privacy_state.viewing_chat.set(true);
        next_tick().await;
        let field = privacy_container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        field.set_value("Private draft");
        field
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(field.value(), "Private draft");
        assert_no_capture();
        for status in [
            ConnectionStatus::Disconnected,
            ConnectionStatus::Connecting,
            ConnectionStatus::Bootstrapping,
            ConnectionStatus::Error("fixture connection failure".to_owned()),
            ConnectionStatus::Connected,
        ] {
            privacy_state.connection_statuses.update(|statuses| {
                statuses.insert(host.clone(), status);
            });
            next_tick().await;
            assert_eq!(field.value(), "Private draft");
            assert_no_capture();
        }
        drop(privacy_mount);
        privacy_container.remove();
        drop(platform_override);
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
                review_recipient: false,
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
        next_tick().await;
        let state = handle.borrow().as_ref().unwrap().clone();
        let legacy_agent_ref = crate::state::AgentRef {
            local_host_id: host.clone(),
            agent_id: protocol::AgentId("agent-1".to_owned()),
        };
        state.agent_compactions.update(|compactions| {
            compactions.insert(
                legacy_agent_ref.clone(),
                protocol::types::AgentCompactNotifyPayload {
                    status: protocol::types::AgentCompactStatus::Completed,
                    old_agent_id: legacy_agent_ref.agent_id.clone(),
                    old_session_id: Some(protocol::SessionId("old-session".to_owned())),
                    new_agent_id: Some(protocol::AgentId("replacement-agent".to_owned())),
                    new_session_id: Some(protocol::SessionId("new-session".to_owned())),
                    summary_preview: None,
                    message: None,
                },
            );
        });

        // A message the user discarded *before* the drop. Its tombstone is the only
        // thing stopping the tool card that sent it from going back to claiming it
        // is on its way to the agent — so a disconnect must not take it.
        let discarded = crate::state::SubmissionOriginId(999);
        state.hold_submission(crate::state::PendingSubmission {
            human_input_id: protocol::HumanInputId(uuid::Uuid::new_v4().to_string()),
            failure_reason: None,
            action: protocol::HumanInputAction::Send,
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
            human_input_id: protocol::HumanInputId(uuid::Uuid::new_v4().to_string()),
            failure_reason: None,
            action: protocol::HumanInputAction::Send,
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
        assert!(
            state
                .agent_compactions
                .with_untracked(|compactions| !compactions.contains_key(&legacy_agent_ref)),
            "disconnect clears the non-bootstrap legacy redirect from the dead snapshot"
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
        drop(mount);
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
        assert_eq!(error.code, MobileAccessErrorCode::TransportFailed);
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
                        AppMode::HostPicker => view! { <components::PairedHostsPicker /> }.into_any(),
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
