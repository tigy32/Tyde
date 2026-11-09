use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::bridge::AuthProvider;
use crate::components::ui::{StatusDot, StatusTone};
use crate::state::{AppMode, AppState, ConnectionStatus, PairingScreen};

/// Minimum touch-target size, inline so the recovery control stays tappable
/// independently of the stylesheet.
const RECONNECT_TOUCH_TARGET: &str = "min-width:44px;min-height:44px;";

/// Reconnect the active host.
///
/// This is the host-scoped terminal recovery. It lives in the banner because the
/// banner is mounted for the whole workspace and survives every navigation — and
/// a dropped connection has to remain fixable no matter which screen the user
/// happens to be on when it happens.
#[component]
fn HostReconnectButton() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let reconnecting = RwSignal::new(false);

    let on_reconnect = move |_| {
        let Some(host) = state.active_local_host_id.get_untracked() else {
            return;
        };
        log::info!("mobile_connection_control host={host} control=Reconnect button=clicked");
        reconnecting.set(true);
        spawn_local(async move {
            if let Err(error) = bridge::reconnect_paired_host(&host).await {
                log::error!("reconnect: reconnect_paired_host({host}) failed: {error}");
            }
            let _ = reconnecting.try_set(false);
        });
    };

    view! {
        <button
            type="button"
            class="connection-banner-reconnect"
            style=RECONNECT_TOUCH_TARGET
            data-mobile-test="connection-banner-reconnect"
            aria-label="Reconnect to this host"
            disabled=move || reconnecting.get()
            on:click=on_reconnect
        >
            {move || if reconnecting.get() { "Reconnecting…" } else { "Reconnect" }}
        </button>
    }
}

fn native_bundle_message(state: &AppState) -> Option<String> {
    match state.bundle_status.get() {
        // A fixed native package already exposes its typed compatibility rejection.
        crate::bundle::BundleStatus::Unavailable(
            crate::bridge::loader::UnavailableReason::NativeVersion,
        ) if crate::native::is_native() => None,
        status => status.message(),
    }
}

#[component]
fn HostRepairButton() -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        <button type="button" class="connection-banner-action" data-mobile-test="connection-banner-repair"
            on:click=move |_| state.app_mode.set(AppMode::Pairing(PairingScreen::Scanner))>
            "Re-pair"
        </button>
    }
}

#[component]
pub(crate) fn NativeCommandErrorRecovery() -> impl IntoView {
    let state = expect_context::<AppState>();
    view! { {move || {
        if !crate::native::is_native() || !matches!(state.app_mode.get(), AppMode::Workspace | AppMode::HostPicker) {
            return ().into_any();
        }
        match state.active_host_connection_status() {
            ConnectionStatus::NeedsAction { code, .. } if crate::state::needs_tyggs_sign_in(code) => connection_banner_auth_provider_buttons(),
            ConnectionStatus::NeedsAction { .. } => view! { <HostRepairButton /> }.into_any(),
            ConnectionStatus::Error(_) | ConnectionStatus::Disconnected | ConnectionStatus::Connecting => view! { <HostReconnectButton /> }.into_any(),
            ConnectionStatus::Connected | ConnectionStatus::Bootstrapping | ConnectionStatus::UpdateRequired { .. } => ().into_any(),
        }
    } } }
}

fn chat_owns_connection_status(state: &AppState) -> bool {
    crate::native::is_native()
        && crate::native::chat_visible(state)
        && state.native_chat_header_owner.get()
            == Some(mobile_shell_types::native::NativeChatHeaderOwner::Client)
}

pub(crate) fn connection_pending(status: &ConnectionStatus) -> bool {
    matches!(
        status,
        ConnectionStatus::Connecting | ConnectionStatus::Bootstrapping
    )
}

#[component]
fn ReconnectingHostStatus() -> impl IntoView {
    view! {
        <div class="connection-banner-inner connected reconnecting" role="status">
            <StatusDot label="Reconnecting".to_string() tone=StatusTone::Pending
                data_mobile_test="connection-banner-dot-reconnecting" />
            <span class="status-text">"Reconnecting"</span>
        </div>
    }
}

type RestoreReconnectSelection = Box<dyn Fn() -> Result<(), JsValue>>;

fn capture_reconnect_selection(
    dialog: &web_sys::HtmlDialogElement,
) -> Result<Option<RestoreReconnectSelection>, JsValue> {
    let window = web_sys::window()
        .ok_or_else(|| JsValue::from_str("Reconnect dialog window is unavailable"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("Reconnect dialog document is unavailable"))?;
    let Some(active) = document.active_element() else {
        return Ok(None);
    };
    if let Some(textarea) = active.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        let start = textarea
            .selection_start()?
            .ok_or_else(|| JsValue::from_str("Editor selection start is unavailable"))?;
        let end = textarea
            .selection_end()?
            .ok_or_else(|| JsValue::from_str("Editor selection end is unavailable"))?;
        let direction = textarea
            .selection_direction()?
            .ok_or_else(|| JsValue::from_str("Editor selection direction is unavailable"))?;
        let textarea = textarea.clone();
        return Ok(Some(Box::new(move || {
            if textarea.is_connected()
                && document
                    .active_element()
                    .is_some_and(|active| active.is_same_node(Some(textarea.as_ref())))
            {
                textarea.set_selection_range_with_direction(start, end, &direction)?;
            }
            Ok(())
        })));
    }
    if let Some(input) = active.dyn_ref::<web_sys::HtmlInputElement>()
        && let Some(start) = input.selection_start()?
    {
        let end = input
            .selection_end()?
            .ok_or_else(|| JsValue::from_str("Editor selection end is unavailable"))?;
        let direction = input
            .selection_direction()?
            .ok_or_else(|| JsValue::from_str("Editor selection direction is unavailable"))?;
        let input = input.clone();
        return Ok(Some(Box::new(move || {
            if input.is_connected()
                && document
                    .active_element()
                    .is_some_and(|active| active.is_same_node(Some(input.as_ref())))
            {
                input.set_selection_range_with_direction(start, end, &direction)?;
            }
            Ok(())
        })));
    }
    let Some(selection) = window.get_selection()? else {
        return Ok(None);
    };
    if selection.range_count() == 0 {
        return Ok(None);
    }
    let anchor = selection
        .anchor_node()
        .ok_or_else(|| JsValue::from_str("Document selection anchor is unavailable"))?;
    let focus = selection
        .focus_node()
        .ok_or_else(|| JsValue::from_str("Document selection focus is unavailable"))?;
    let anchor_offset = selection.anchor_offset();
    let focus_offset = selection.focus_offset();
    let dialog = dialog.clone();
    #[cfg(debug_assertions)]
    web_sys::console::debug_1(
        &format!(
            "reconnect selection capture: document, collapsed={}",
            selection.is_collapsed()
        )
        .into(),
    );
    Ok(Some(Box::new(move || {
        #[cfg(debug_assertions)]
        web_sys::console::debug_1(&format!(
            "reconnect selection restore: anchor_connected={}, focus_connected={}, same_focus={}, neutral_focus={}, owned_dialog_focus={}, collapsed={}",
            anchor.is_connected(), focus.is_connected(),
            document.active_element().is_some_and(|current| current.is_same_node(Some(active.as_ref()))),
            document.active_element().is_some_and(|current| document.body().is_some_and(|body| current.is_same_node(Some(body.as_ref())))),
            document.active_element().is_some_and(|current| dialog.contains(Some(current.as_ref()))),
            selection.is_collapsed(),
        ).into());
        if anchor.is_connected()
            && focus.is_connected()
            && document.active_element().is_some_and(|current| {
                current.is_same_node(Some(active.as_ref()))
                    || dialog.contains(Some(current.as_ref()))
            })
        {
            // An unfocusable return target can leave focus on our closed dialog's button.
            selection.set_base_and_extent(&anchor, anchor_offset, &focus, focus_offset)?;
        }
        Ok(())
    })))
}

#[component]
pub(crate) fn ReconnectOverlay() -> impl IntoView {
    let state = expect_context::<AppState>();
    let dismissed = RwSignal::new(None);
    let status_state = state.clone();
    let status_pending =
        Memo::new(move |_| connection_pending(&status_state.active_host_connection_status()));
    let pending_state = state.clone();
    let pending = Memo::new(move |_| {
        matches!(
            pending_state.app_mode.get(),
            AppMode::Workspace | AppMode::HostPicker
        ) && connection_pending(&pending_state.active_host_connection_status())
            && native_bundle_message(&pending_state).is_none()
            && pending_state.active_host_command_error().is_none()
            && pending_state.mobile_shell_error.get().is_none()
    });
    let host = state.active_local_host_id;
    Effect::new(move |_| {
        let active = host.get();
        if !status_pending.get() || dismissed.get_untracked() != active {
            log::debug!("mobile reconnect dismissal reset by authoritative connection/host change");
            dismissed.set(None);
        }
    });
    let open = Signal::derive(move || {
        pending.get() && host.get().is_some() && dismissed.get() != host.get()
    });
    crate::native::track_modal(open);
    let dialog = NodeRef::<leptos::html::Dialog>::new();
    let dialog_closing = RwSignal::new(false);
    let selection = StoredValue::new_local(None::<RestoreReconnectSelection>);
    let shell_error = state.mobile_shell_error;
    let report = move |message: &'static str| {
        shell_error.set(Some(crate::state::MobileShellError {
            code: protocol::MobileAccessErrorCode::Internal,
            message: message.into(),
        }));
    };
    Effect::new(move |_| {
        let Some(dialog) = dialog.get() else {
            return;
        };
        let should_open = open.get();
        if dialog_closing.get() {
            return;
        }
        if should_open {
            if !dialog.open() {
                match capture_reconnect_selection(&dialog) {
                    Ok(saved) => selection.set_value(saved),
                    Err(error) => {
                        log::error!("Reconnect dialog could not preserve selection: {error:?}");
                        report("Reconnect dialog could not preserve selection.");
                        return;
                    }
                }
                if let Err(error) = dialog.show_modal() {
                    log::error!("Reconnect dialog could not open: {error:?}");
                    selection.set_value(None);
                    report("Reconnect dialog could not open.");
                }
            }
        } else if dialog.open() {
            log::debug!("mobile reconnect overlay close requested; presentation only");
            dialog_closing.set(true);
            dialog.close();
        }
    });
    view! {
        <dialog node_ref=dialog class="reconnect-dialog" aria-labelledby="reconnect-dialog-title"
            data-mobile-test="reconnect-dialog"
            on:close=move |_: web_sys::Event| {
                // Native close completion, not close()'s transitional focus, owns restoration.
                let restored = selection.with_value(|saved| match saved {
                    Some(restore) => restore(),
                    None => Ok(()),
                });
                selection.set_value(None);
                if let Err(error) = restored {
                    log::error!("Reconnect dialog could not restore selection: {error:?}");
                    report("Reconnect dialog could not restore selection.");
                }
                log::debug!("mobile reconnect overlay close completed; presentation only");
                dialog_closing.set(false);
            }
            on:cancel=move |event: web_sys::Event| {
                event.prevent_default();
                dismissed.set(host.get_untracked());
            }>
            <h2 id="reconnect-dialog-title">"Reconnecting"</h2>
            <button type="button" autofocus data-mobile-test="reconnect-dismiss"
                on:click=move |_| dismissed.set(host.get_untracked())>"Dismiss"</button>
        </dialog>
    }
}

#[component]
fn ConnectedHostStatus() -> impl IntoView {
    let state = expect_context::<AppState>();
    view! { {move || {
                        let round_trip_ms = state
                            .active_local_host_id
                            .get()
                            .and_then(|host| {
                                state
                                    .heartbeat_round_trip_ms_by_host
                                    .get()
                                    .get(&host)
                                    .copied()
                            });
                        let round_trip_label = round_trip_ms
                            .map(|milliseconds| format!("{milliseconds} ms"))
                            .unwrap_or_else(|| "Checking…".to_owned());
                        let aria_label = round_trip_ms
                            .map(|milliseconds| {
                                format!("Connected, {milliseconds} millisecond round trip")
                            })
                            .unwrap_or_else(|| "Connected, checking round-trip time".to_owned());
                        view! {
                            <div class="connection-banner-inner connected" aria-label=aria_label>
                                <StatusDot
                                    label="Connected".to_string()
                                    tone=StatusTone::Online
                                    data_mobile_test="connection-banner-dot-connected"
                                />
                                <span
                                    class="status-text"
                                    data-mobile-test="connection-banner-rtt"
                                >
                                    {round_trip_label}
                                </span>
                            </div>
                        }.into_any()
    }} }
}

#[component]
pub(crate) fn ChatConnectionStatus() -> impl IntoView {
    let state = expect_context::<AppState>();
    let pending = Memo::new(move |_| connection_pending(&state.active_host_connection_status()));
    let visible_state = expect_context::<AppState>();
    view! { <Show when=move || chat_owns_connection_status(&visible_state)
        && (matches!(visible_state.active_host_connection_status(), ConnectionStatus::Connected)
            || connection_pending(&visible_state.active_host_connection_status()))
        && native_bundle_message(&visible_state).is_none()
        && visible_state.active_host_command_error().is_none()
        && visible_state.mobile_shell_error.get().is_none()>
        <span class="chat-connection-status">{move || if pending.get() {
            view! { <ReconnectingHostStatus /> }.into_any()
        } else { view! { <ConnectedHostStatus /> }.into_any() }}</span>
    </Show> }
}

/// Floating connection indicator. Healthy connections show a compact
/// end-to-end round-trip measurement; states needing attention expand to a
/// larger action pill.
#[component]
pub fn ConnectionBanner(#[prop(optional)] inline: bool) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let status_state = state.clone();
    let command_error_state = state.clone();
    let bundle_state = state.clone();

    view! {
        <div class=if inline { "connection-banner board-connection" } else { "connection-banner" } data-mobile-test="connection-banner"
            hidden=move || (!inline && state.active_tab.get() == crate::state::MobileTab::Agents
                && !state.viewing_chat.get()
                && state.board_project.get().is_some_and(|target| Some(target.local_host_id) == state.active_local_host_id.get())) || (crate::native::is_native() && state.mobile_shell_error.get().is_some())
                || ((chat_owns_connection_status(&state) || crate::native::root_status_owned(&state))
                    && (matches!(state.active_host_connection_status(), ConnectionStatus::Connected)
                        || (chat_owns_connection_status(&state) && connection_pending(&state.active_host_connection_status())))
                    && native_bundle_message(&state).is_none()
                    && state.active_host_command_error().is_none())>
            {move || native_bundle_message(&bundle_state).map(|message| view! {
                <div class="connection-banner-inner error" role="status" data-mobile-test="bundle-sync">
                    <span class="status-text">{message}</span>
                </div>
            })}
            {move || {
                if crate::native::is_native()
                    && (native_bundle_message(&status_state).is_some() || status_state.active_host_command_error().is_some()) {
                    return ().into_any();
                }
                let status = status_state.active_host_connection_status();
                match status {
                    ConnectionStatus::Connected => {
                        if chat_owns_connection_status(&status_state) {
                            ().into_any()
                        } else {
                            view! { <ConnectedHostStatus /> }.into_any()
                        }
                    }
                    ConnectionStatus::Connecting | ConnectionStatus::Bootstrapping => {
                        if chat_owns_connection_status(&status_state) {
                            ().into_any()
                        } else {
                            view! { <ReconnectingHostStatus /> }.into_any()
                        }
                    }
                    ConnectionStatus::Disconnected => {
                        // Disconnected is a terminal state: nothing else on screen
                        // is going to fix it, and it survives wherever the user
                        // navigates. It used to be a dot and a label — a statement
                        // of fact with nothing to do about it. Give it the action.
                        view! {
                            <div class="connection-banner-inner disconnected" role="status">
                                <StatusDot
                                    label="Disconnected".to_string()
                                    tone=StatusTone::Muted
                                    data_mobile_test="connection-banner-dot-disconnected"
                                />
                                <span class="status-text">"Disconnected"</span>
                                <HostReconnectButton />
                            </div>
                        }.into_any()
                    }
                    ConnectionStatus::Error(ref msg) => {
                        let msg = msg.clone();
                        view! {
                            <div class="connection-banner-inner error connection-failure" role="alert">
                                <StatusDot
                                    label="Error".to_string()
                                    tone=StatusTone::Error
                                    data_mobile_test="connection-banner-dot-error"
                                />
                                <span class="status-text">{msg}</span>
                                <HostReconnectButton />

                            </div>
                        }.into_any()
                    }
                    // Sticky, terminal update-required state: the host speaks a
                    // protocol this build cannot. No dismiss — clearing it would
                    // just leave a blank skeleton behind an unusable connection.
                    // The web/PWA loader self-heals by rebooting into the host's
                    // published bundle.
                    ConnectionStatus::UpdateRequired { host_protocol, app_protocol, release_version } => {
                        let message = if crate::native::is_native() {
                            "Update this app and host together to connect.".into()
                        } else {
                            crate::state::update_required_message(host_protocol, app_protocol, release_version.as_ref())
                        };
                        view! {
                            <div
                                class="connection-banner-inner error"
                                role="alert"
                                data-mobile-test="connection-banner-update-required"
                            >
                                <StatusDot
                                    label="Update required".to_string()
                                    tone=StatusTone::Error
                                    data_mobile_test="connection-banner-dot-update-required"
                                />
                                <span class="status-text">{message}</span>
                            </div>
                        }.into_any()
                    }
                    // Sticky, terminal managed-access failure. The connection actor
                    // has stopped retrying, so there is no dismiss — the user must
                    // sign in with Tyggs again or re-pair. Sign-in navigates to the
                    // tycode.dev OAuth redirect; re-pair happens from the host list.
                    ConnectionStatus::NeedsAction { code, message } => {
                        let show_sign_in = crate::state::needs_tyggs_sign_in(code);
                        view! {
                            <div
                                class="connection-banner-inner error"
                                role="alert"
                                data-mobile-test="connection-banner-needs-action"
                            >
                                <StatusDot
                                    label="Action required".to_string()
                                    tone=StatusTone::Error
                                    data_mobile_test="connection-banner-dot-needs-action"
                                />
                                <span class="status-text">{message}</span>
                                {if show_sign_in {
                                    // ServiceAuthRequired / PassRequired → navigate to
                                    // the tycode.dev-hosted Tyggs OAuth redirect.
                                    connection_banner_auth_provider_buttons()
                                } else {
                                    view! { <HostRepairButton /> }.into_any()
                                }}
                            </div>
                        }.into_any()
                    }
                }
            }}
            {move || {
                let error = if crate::native::is_native() && native_bundle_message(&command_error_state).is_some() {
                    None
                } else { command_error_state.active_host_command_error() };
                error.map(|msg| {
                    let dismiss_state = command_error_state.clone();
                    view! {
                        <div class="command-error-banner" role="alert">
                            <span class="error-text">{msg}</span>
                            <NativeCommandErrorRecovery />
                            <button
                                type="button"
                                class="command-error-dismiss error-banner-dismiss"
                                data-mobile-test="command-error-dismiss"
                                aria-label="Dismiss command error"
                                on:click=move |_| dismiss_active_command_error(&dismiss_state)
                            >
                                "×"
                            </button>
                        </div>
                    }
                })
            }}
        </div>
    }
}

fn connection_banner_auth_provider_buttons() -> AnyView {
    let native_sign_in_state = expect_context::<AppState>();
    match crate::bridge::tyggs_auth_providers() {
        Ok(providers) => view! {
            <span class="connection-banner-provider-actions">
                {providers
                    .into_iter()
                    .enumerate()
                    .map(|(index, provider)| {
                        let test_id = if index == 0 {
                            "connection-banner-sign-in"
                        } else {
                            connection_banner_auth_provider_test_id(provider)
                        };
                        let native_state = native_sign_in_state.clone();
                        view! {
                            <button
                                type="button"
                                class="connection-banner-action"
                                data-mobile-test=test_id
                                data-mobile-auth-provider=provider.as_str()
                                aria-label=provider.sign_in_label()
                                on:click=move |_| {
                                    if crate::native::is_native() {
                                        if let Some(host) = native_state.active_local_host_id.get_untracked() {
                                            crate::actions::native_sign_in_and_reconnect(native_state.clone(), host, provider);
                                        } else {
                                            native_state.mobile_shell_error.set(Some(crate::state::MobileShellError {
                                                code: protocol::MobileAccessErrorCode::Internal,
                                                message: "No host is selected for sign-in".into(),
                                            }));
                                        }
                                        return;
                                    }
                                    if let Err(error) = crate::bridge::begin_tyggs_sign_in(provider, None) {
                                        log::error!("failed to start Tyggs sign-in: {error}");
                                    }
                                }
                            >
                                {provider.sign_in_label()}
                            </button>
                        }
                    })
                    .collect::<Vec<_>>()}
            </span>
        }
        .into_any(),
        Err(message) => view! {
            <span
                class="connection-banner-action-error"
                role="alert"
                data-mobile-test="connection-banner-provider-error"
            >
                {message}
            </span>
        }
        .into_any(),
    }
}

fn connection_banner_auth_provider_test_id(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::Apple => "connection-banner-sign-in-apple",
        AuthProvider::Google => "connection-banner-sign-in-google",
    }
}

fn dismiss_active_command_error(state: &AppState) {
    let Some(host) = state.active_local_host_id.get_untracked() else {
        return;
    };
    state.command_errors_by_host.update(|errors| {
        errors.remove(&host);
    });
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    use crate::state::LocalHostId;

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

    fn set_service_config(json: &str) {
        let window = web_sys::window().expect("window");
        let key = wasm_bindgen::JsValue::from_str("__TYDE_MOBILE_SERVICE__");
        if json.is_empty() {
            let _ = js_sys::Reflect::delete_property(&window, &key);
            return;
        }
        let value = js_sys::JSON::parse(json).expect("parse service config");
        js_sys::Reflect::set(&window, &key, &value).expect("install service config");
    }

    #[wasm_bindgen_test]
    async fn connected_status_shows_round_trip_time() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let state = AppState::new();
        state.active_local_host_id.set(Some(host.clone()));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), ConnectionStatus::Connected);
        });
        state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
            round_trips.insert(host, 42);
        });
        let mount_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            view! { <ConnectionBanner /><ChatConnectionStatus /> }
        });
        next_tick().await;

        let dot = container
            .query_selector("[data-mobile-test='connection-banner-dot-connected']")
            .unwrap()
            .expect("connected indicator should render the status dot");
        assert_eq!(
            dot.get_attribute("aria-label").as_deref(),
            Some("Connected")
        );
        let round_trip = container
            .query_selector("[data-mobile-test='connection-banner-rtt']")
            .unwrap()
            .expect("connected indicator should render round-trip feedback");
        assert_eq!(round_trip.text_content().as_deref(), Some("42 ms"));
        state.viewing_chat.set(true);
        next_tick().await;
        assert!(
            container
                .query_selector(".chat-connection-status")
                .unwrap()
                .is_none(),
            "PWA chat must not claim the native inline status location"
        );
        let banner = container
            .query_selector("[data-mobile-test='connection-banner']")
            .unwrap()
            .unwrap();
        assert!(
            !banner.has_attribute("hidden"),
            "PWA retains its floating latency pill while viewing chat"
        );
        assert_eq!(round_trip.text_content().as_deref(), Some("42 ms"));
    }

    #[wasm_bindgen_test]
    async fn bootstrapping_status_is_visible() {
        let host = LocalHostId("host-bootstrapping".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(host_for_mount.clone(), ConnectionStatus::Bootstrapping);
            });
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='connection-banner-dot-reconnecting']")
                .unwrap()
                .is_some(),
            "Bootstrapping retains the pending indicator until authoritative connected state"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Reconnecting"),
            "Host bootstrap must not prematurely show connected"
        );
    }

    #[wasm_bindgen_test]
    async fn terminal_connection_error_stays_visible() {
        crate::components::test_styles::ensure_styles_loaded();
        let host = LocalHostId("host-error".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        container.set_class_name("mobile-app");
        container
            .style()
            .set_css_text("width:390px;height:800px;position:relative");
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state
                .paired_hosts
                .set(vec![crate::state::PairedHostSummary {
                    local_host_id: host_for_mount.clone(),
                    host_label: "Owned host".into(),
                    direct_origin: None,
                    credential_fingerprint: "public-layout-fixture".into(),
                    auto_connect: false,
                    last_connected_at_ms: None,
                }]);
            state.connection_statuses.update(|statuses| {
                statuses.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::Error("host unreachable".to_owned()),
                );
            });
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ConnectionBanner /><div class="mobile-content"><crate::components::HomeView /></div> }
        });
        next_tick().await;

        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("host unreachable"),
            "connection error should render before dismissal"
        );
        let state = state_handle.borrow().as_ref().unwrap().clone();
        for width in [320, 390, 466] {
            container
                .style()
                .set_property("width", &format!("{width}px"))
                .unwrap();
            for cause in [
                "Mobile service unavailable",
                "The host did not answer.",
                crate::bridge::loader::UnavailableReason::HostUnavailable.message(),
            ] {
                state.connection_statuses.update(|statuses| {
                    statuses.insert(host.clone(), ConnectionStatus::Error(cause.into()));
                });
                next_tick().await;
                let alert = container.query_selector("[role='alert']").unwrap().unwrap();
                let heading = container.query_selector("h1").unwrap().unwrap();
                let message = alert.query_selector(".status-text").unwrap().unwrap();
                let reconnect = alert
                    .query_selector("[data-mobile-test='connection-banner-reconnect']")
                    .unwrap()
                    .unwrap();
                let alert_box = alert.get_bounding_client_rect();
                let text_box = message.get_bounding_client_rect();
                let reconnect_box = reconnect.get_bounding_client_rect();
                assert_eq!(message.text_content().as_deref(), Some(cause));
                if cause == crate::bridge::loader::UnavailableReason::HostUnavailable.message() {
                    assert_eq!(
                        message.text_content().as_deref(),
                        Some("the host's mobile access registration is unavailable")
                    );
                    assert!(
                        alert
                            .query_selector("[data-mobile-test='connection-banner-repair']")
                            .unwrap()
                            .is_none(),
                        "Unavailable registration must not ask to re-pair"
                    );
                }
                assert!(
                    alert_box.bottom() <= heading.get_bounding_client_rect().top(),
                    "A failure must reserve space above the host heading"
                );
                assert!(
                    text_box.right() <= reconnect_box.left(),
                    "Cause and recovery controls must not overlap"
                );
                assert!(reconnect_box.height() >= 44.0, "Recovery stays tappable");
                assert!(
                    alert
                        .query_selector("[data-mobile-test='connection-error-dismiss']")
                        .unwrap()
                        .is_none(),
                    "Terminal failures cannot be dismissed"
                );
                assert!(
                    message.scroll_width() <= message.client_width()
                        && message.scroll_height() <= message.client_height(),
                    "The complete cause must remain readable"
                );
            }
        }
        assert!(matches!(
            state.active_host_connection_status(),
            ConnectionStatus::Error(_)
        ));
    }

    #[wasm_bindgen_test]
    async fn update_required_renders_actionable_message_without_dismiss() {
        let host = LocalHostId("host-update-required".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::UpdateRequired {
                        host_protocol: 31,
                        app_protocol: 30,
                        release_version: Some(
                            protocol::TydeReleaseVersion::parse("0.8.19-beta.15").unwrap(),
                        ),
                    },
                );
            });
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        let alert = container
            .query_selector("[data-mobile-test='connection-banner-update-required']")
            .unwrap()
            .expect("update-required alert must render");
        let alert_text = alert.text_content().unwrap_or_default();
        assert!(
            alert_text.contains("0.8.19-beta.15"),
            "the reject's host build must be named to the user: {alert_text}"
        );
        assert!(
            alert_text.contains("protocol 31") && alert_text.contains("app protocol 30"),
            "the actionable protocol mismatch must be shown to the user: {alert_text}"
        );
        // Terminal/sticky: no dismiss button — clearing it would only reveal a
        // blank skeleton behind an unusable connection.
        assert!(
            container
                .query_selector("[data-mobile-test='connection-error-dismiss']")
                .unwrap()
                .is_none(),
            "update-required is sticky and must not offer a dismiss control"
        );
    }

    /// A `RepairRequired` managed failure offers a Re-pair action in the banner
    /// (not a Sign-in), and tapping it routes to the pairing scanner.
    #[wasm_bindgen_test]
    async fn needs_action_repair_offers_repair_action() {
        let host = LocalHostId("host-repair".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::RepairRequired,
                        message: "Re-pair from the host's current QR code.".to_owned(),
                    },
                );
            });
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        let repair: HtmlElement = container
            .query_selector("[data-mobile-test='connection-banner-repair']")
            .unwrap()
            .expect("repair-required banner must offer a Re-pair action")
            .dyn_into()
            .unwrap();
        assert!(
            container
                .query_selector("[data-mobile-test='connection-banner-sign-in']")
                .unwrap()
                .is_none(),
            "a repair (non-auth) failure must not offer a sign-in action"
        );
        repair.click();
        next_tick().await;

        let state = state_handle.borrow().as_ref().unwrap().clone();
        assert!(
            matches!(
                state.app_mode.get_untracked(),
                AppMode::Pairing(PairingScreen::Scanner)
            ),
            "tapping Re-pair must route to the pairing scanner"
        );
    }

    /// A sign-in-required managed failure offers Sign in (not Re-pair).
    #[wasm_bindgen_test]
    async fn needs_action_auth_offers_sign_in_action() {
        let host = LocalHostId("host-auth".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::ServiceAuthRequired,
                        message: "Sign in with Tyggs again.".to_owned(),
                    },
                );
            });
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='connection-banner-sign-in']")
                .unwrap()
                .is_some(),
            "an auth-required failure must offer a sign-in action"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='connection-banner-repair']")
                .unwrap()
                .is_none(),
            "an auth-required failure must not offer a Re-pair action"
        );
    }

    #[wasm_bindgen_test]
    async fn needs_action_auth_renders_provider_buttons_in_order() {
        set_service_config(r#"{"providers":["apple","google"]}"#);
        let host = LocalHostId("host-auth-providers".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::ServiceAuthRequired,
                        message: "Sign in with Tyggs again.".to_owned(),
                    },
                );
            });
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        let legacy: HtmlElement = container
            .query_selector("[data-mobile-test='connection-banner-sign-in']")
            .unwrap()
            .expect("legacy selector must point at the first provider button")
            .dyn_into()
            .unwrap();
        assert_eq!(legacy.tag_name(), "BUTTON");
        assert_eq!(
            legacy.get_attribute("data-mobile-auth-provider").as_deref(),
            Some("apple")
        );
        let apple: HtmlElement = container
            .query_selector("[data-mobile-auth-provider='apple']")
            .unwrap()
            .expect("Apple sign-in button")
            .dyn_into()
            .unwrap();
        let google: HtmlElement = container
            .query_selector("[data-mobile-auth-provider='google']")
            .unwrap()
            .expect("Google sign-in button")
            .dyn_into()
            .unwrap();
        assert_eq!(apple.tag_name(), "BUTTON");
        assert_eq!(google.tag_name(), "BUTTON");
        assert_eq!(
            google.get_attribute("data-mobile-test").as_deref(),
            Some("connection-banner-sign-in-google")
        );
        let text = container.text_content().unwrap_or_default();
        let apple_index = text.find("Continue with Apple").expect("Apple label");
        let google_index = text.find("Continue with Google").expect("Google label");
        assert!(
            apple_index < google_index,
            "provider buttons must follow config order: {text}"
        );

        set_service_config("");
    }

    #[wasm_bindgen_test]
    async fn command_error_can_be_dismissed() {
        let host = LocalHostId("host-command-error".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.command_errors_by_host.update(|errors| {
                errors.insert(host_for_mount.clone(), "SpawnAgent failed".to_owned());
            });
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ConnectionBanner /> }
        });
        next_tick().await;

        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("SpawnAgent failed"),
            "command error should render before dismissal"
        );
        let dismiss: HtmlElement = container
            .query_selector("[data-mobile-test='command-error-dismiss']")
            .unwrap()
            .expect("command error dismiss should render")
            .dyn_into()
            .unwrap();
        dismiss.click();
        next_tick().await;

        let state = state_handle.borrow().as_ref().unwrap().clone();
        assert!(
            !state
                .command_errors_by_host
                .get_untracked()
                .contains_key(&host),
            "dismissing should remove the active host command error"
        );
        assert!(
            !container
                .text_content()
                .unwrap_or_default()
                .contains("SpawnAgent failed"),
            "command error text should disappear after dismissal"
        );
    }
}
