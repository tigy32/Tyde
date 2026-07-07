use leptos::prelude::*;

use crate::components::ui::{Spinner, StatusDot, StatusTone};
use crate::state::{AppMode, AppState, ConnectionStatus, PairingScreen};

/// Floating connection indicator. The healthy connected state stays
/// dot-only so it does not take vertical space from the workspace; states
/// needing attention expand to a compact pill.
#[component]
pub fn ConnectionBanner() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let status_state = state.clone();
    let command_error_state = state.clone();

    view! {
        <div class="connection-banner" data-mobile-test="connection-banner">
            {move || {
                let status = status_state.active_host_connection_status();
                match status {
                    ConnectionStatus::Connected => {
                        view! {
                            <div class="connection-banner-inner connected" aria-label="Connected">
                                <StatusDot
                                    label="Connected".to_string()
                                    tone=StatusTone::Online
                                    data_mobile_test="connection-banner-dot-connected"
                                />
                            </div>
                        }.into_any()
                    }
                    ConnectionStatus::Connecting => {
                        view! {
                            <div class="connection-banner-inner connecting">
                                <Spinner
                                    data_mobile_test="connection-banner-spinner"
                                    aria_label="Connecting to host".to_string()
                                />
                                <span class="status-text">"Connecting…"</span>
                            </div>
                        }.into_any()
                    }
                    ConnectionStatus::Bootstrapping => {
                        view! {
                            <div class="connection-banner-inner connecting" data-mobile-test="connection-banner-bootstrapping">
                                <Spinner
                                    data_mobile_test="connection-banner-bootstrap-spinner"
                                    aria_label="Loading host state".to_string()
                                />
                                <span class="status-text">"Loading host…"</span>
                            </div>
                        }.into_any()
                    }
                    ConnectionStatus::Disconnected => {
                        view! {
                            <div class="connection-banner-inner disconnected">
                                <StatusDot
                                    label="Disconnected".to_string()
                                    tone=StatusTone::Muted
                                    data_mobile_test="connection-banner-dot-disconnected"
                                />
                                <span class="status-text">"Disconnected"</span>
                            </div>
                        }.into_any()
                    }
                    ConnectionStatus::Error(ref msg) => {
                        let msg = msg.clone();
                        let dismiss_state = status_state.clone();
                        view! {
                            <div class="connection-banner-inner error" role="alert">
                                <StatusDot
                                    label="Error".to_string()
                                    tone=StatusTone::Error
                                    data_mobile_test="connection-banner-dot-error"
                                />
                                <span class="status-text">{msg}</span>
                                <button
                                    type="button"
                                    class="connection-banner-dismiss error-banner-dismiss"
                                    data-mobile-test="connection-error-dismiss"
                                    aria-label="Dismiss connection error"
                                    on:click=move |_| dismiss_active_connection_error(&dismiss_state)
                                >
                                    "×"
                                </button>
                            </div>
                        }.into_any()
                    }
                    // Sticky, terminal update-required state: the host speaks a
                    // protocol this build cannot. No dismiss — clearing it would
                    // just leave a blank skeleton behind an unusable connection.
                    // The web/PWA loader self-heals by rebooting into the host's
                    // published bundle; native shells surface this until the app
                    // is updated.
                    ConnectionStatus::UpdateRequired { host_protocol, app_protocol, release_version } => {
                        let message = crate::state::update_required_message(
                            host_protocol,
                            app_protocol,
                            release_version.as_ref(),
                        );
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
                        let repair_state = status_state.clone();
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
                                    view! {
                                        <button
                                            type="button"
                                            class="connection-banner-action"
                                            data-mobile-test="connection-banner-sign-in"
                                            on:click=move |_| {
                                                if let Err(error) = crate::bridge::begin_tyggs_sign_in(None) {
                                                    log::error!("failed to start Tyggs sign-in: {error}");
                                                }
                                            }
                                        >
                                            "Sign in with Tyggs"
                                        </button>
                                    }.into_any()
                                } else {
                                    // RepairRequired → send the user to the scanner to
                                    // re-pair this host through tycode.dev.
                                    view! {
                                        <button
                                            type="button"
                                            class="connection-banner-action"
                                            data-mobile-test="connection-banner-repair"
                                            on:click=move |_| {
                                                repair_state.app_mode.set(
                                                    AppMode::Pairing(PairingScreen::Scanner),
                                                );
                                            }
                                        >
                                            "Re-pair"
                                        </button>
                                    }.into_any()
                                }}
                            </div>
                        }.into_any()
                    }
                }
            }}
            {move || {
                let error = command_error_state.active_host_command_error();
                error.map(|msg| {
                    let dismiss_state = command_error_state.clone();
                    view! {
                        <div class="command-error-banner" role="alert">
                            <span class="error-text">{msg}</span>
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

fn dismiss_active_connection_error(state: &AppState) {
    let Some(host) = state.active_local_host_id.get_untracked() else {
        return;
    };
    state.connection_statuses.update(|statuses| {
        if matches!(statuses.get(&host), Some(ConnectionStatus::Error(_))) {
            statuses.insert(host, ConnectionStatus::Disconnected);
        }
    });
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

    #[wasm_bindgen_test]
    async fn connected_status_is_dot_only() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.connection_statuses.update(|statuses| {
                statuses.insert(host_for_mount.clone(), ConnectionStatus::Connected);
            });
            provide_context(state);
            view! { <ConnectionBanner /> }
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
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "connected state should not render a text status bar"
        );
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
                .query_selector("[data-mobile-test='connection-banner-bootstrapping']")
                .unwrap()
                .is_some(),
            "bootstrapping status should render a visible loading banner"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Loading host"),
            "bootstrapping status should distinguish host bootstrap from transport connect"
        );
    }

    #[wasm_bindgen_test]
    async fn connection_error_can_be_dismissed() {
        let host = LocalHostId("host-error".to_owned());
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
                    ConnectionStatus::Error("host unreachable".to_owned()),
                );
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
                .contains("host unreachable"),
            "connection error should render before dismissal"
        );
        let dismiss: HtmlElement = container
            .query_selector("[data-mobile-test='connection-error-dismiss']")
            .unwrap()
            .expect("connection error dismiss should render")
            .dyn_into()
            .unwrap();
        dismiss.click();
        next_tick().await;

        let state = state_handle.borrow().as_ref().unwrap().clone();
        assert_eq!(
            state.connection_statuses.get_untracked().get(&host),
            Some(&ConnectionStatus::Disconnected),
            "dismissing should downgrade the visible connection error"
        );
        assert!(
            !container
                .text_content()
                .unwrap_or_default()
                .contains("host unreachable"),
            "connection error text should disappear after dismissal"
        );
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
