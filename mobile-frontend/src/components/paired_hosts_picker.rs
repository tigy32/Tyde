use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::bridge::AuthProvider;
use crate::components::ui::{Button, ButtonVariant};
use crate::state::{AppMode, AppState, ConnectionStatus, LocalHostId, PairingScreen};

/// Shown when the user has at least one paired host but hasn't picked which
/// to use yet. Lists every paired host with its connection status pill and a
/// Connect/Disconnect affordance. A "Pair another host" button is always
/// visible.
#[component]
pub fn PairedHostsPicker() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let confirming_forget: RwSignal<Option<LocalHostId>> = RwSignal::new(None);

    let state_for_pair = state.clone();
    let on_pair_another = Callback::new(move |_: ()| {
        state_for_pair
            .app_mode
            .set(AppMode::Pairing(PairingScreen::Scanner));
    });

    let state_for_rows = state.clone();

    view! {
        <div class="view picker-view">
            <div class="view-header">
                <h1 class="view-title">"Pick a Host"</h1>
            </div>
            <div class="view-body">
                {move || {
                    let hosts = state_for_rows.paired_hosts.get();
                    if hosts.is_empty() {
                        return view! {
                            <div class="empty-state">
                                <div class="empty-state-text">"No paired hosts"</div>
                            </div>
                        }
                        .into_any();
                    }
                    view! {
                        <div class="paired-host-list">
                            {hosts.into_iter().map(|host| {
                                let id = host.local_host_id.clone();
                                let id_for_status = id.clone();
                                let id_for_connect = id.clone();
                                let id_for_disconnect = id.clone();
                                let id_for_select = id.clone();
                                let id_for_confirm = id.clone();
                                let id_for_forget = id.clone();
                                let label = host.host_label.clone();
                                let state = state_for_rows.clone();
                                let state_for_status = state.clone();
                                let state_for_select = state.clone();
                                let state_for_forget = state.clone();
                                let label_for_confirm = label.clone();

                                view! {
                                    <div class="paired-host-card">
                                        <div class="paired-host-info">
                                            <div class="paired-host-label">{label.clone()}</div>
                                            <div class="paired-host-status">
                                                <ConnectionPill
                                                    state=state_for_status
                                                    local_host_id=id_for_status
                                                />
                                            </div>
                                        </div>
                                        {move || {
                                            if confirming_forget.get().as_ref() == Some(&id_for_confirm) {
                                                let id_for_delete = id_for_forget.clone();
                                                let state = state_for_forget.clone();
                                                view! {
                                                    <div class="paired-host-actions paired-host-actions-confirm">
                                                        <Button
                                                            label="Cancel"
                                                            variant=ButtonVariant::Secondary
                                                            full_width=true
                                                            data_mobile_test="paired-host-forget-cancel"
                                                            on_click=Callback::new(move |_: ()| confirming_forget.set(None))
                                                        />
                                                        <Button
                                                            label="Delete"
                                                            variant=ButtonVariant::Destructive
                                                            full_width=true
                                                            data_mobile_test="paired-host-forget-confirm"
                                                            aria_label=format!("Delete pairing for {}", label_for_confirm)
                                                            on_click=Callback::new(move |_: ()| {
                                                                let id = id_for_delete.clone();
                                                                let state = state.clone();
                                                                spawn_local(async move {
                                                                    if let Err(error) = bridge::forget_paired_host(&id).await {
                                                                        log::error!("forget_paired_host({id}) failed: {error}");
                                                                        state.mobile_shell_error.set(Some(crate::state::MobileShellError {
                                                                            code: protocol::MobileAccessErrorCode::StoreLoadFailed,
                                                                            message: format!("Failed to delete paired host: {error}"),
                                                                        }));
                                                                        return;
                                                                    }
                                                                    state.clear_host_runtime(&id);
                                                                });
                                                            })
                                                        />
                                                    </div>
                                                }.into_any()
                                            } else {
                                                let id_for_confirm_set = id_for_confirm.clone();
                                                view! {
                                                    <div class="paired-host-actions">
                                                        <ConnectDisconnectButton
                                                            state=state.clone()
                                                            local_host_id_for_connect=id_for_connect.clone()
                                                            local_host_id_for_disconnect=id_for_disconnect.clone()
                                                        />
                                                        <Button
                                                            label="Open"
                                                            variant=ButtonVariant::Primary
                                                            full_width=true
                                                            on_click=Callback::new({
                                                                let state_for_select = state_for_select.clone();
                                                                let id_for_select = id_for_select.clone();
                                                                move |_: ()| {
                                                                    state_for_select
                                                                        .active_local_host_id
                                                                        .set(Some(id_for_select.clone()));
                                                                }
                                                            })
                                                        />
                                                        <Button
                                                            label="Delete"
                                                            variant=ButtonVariant::Destructive
                                                            full_width=true
                                                            data_mobile_test="paired-host-forget"
                                                            aria_label=format!("Delete pairing for {}", label)
                                                            on_click=Callback::new(move |_: ()| confirming_forget.set(Some(id_for_confirm_set.clone())))
                                                        />
                                                    </div>
                                                }.into_any()
                                            }
                                        }}
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }
                    .into_any()
                }}
                <Button
                    label="Pair another host"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    class="picker-pair-another"
                    on_click=on_pair_another
                />
            </div>
        </div>
    }
}

#[component]
fn ConnectionPill(state: AppState, local_host_id: LocalHostId) -> impl IntoView {
    let id = local_host_id.clone();
    let pill = move || {
        let status = state
            .connection_statuses
            .get()
            .get(&id)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected);
        match status {
            ConnectionStatus::Connected => ("connected", "Online".to_string()),
            ConnectionStatus::Connecting => ("connecting", "Connecting…".to_string()),
            ConnectionStatus::Bootstrapping => ("connecting", "Loading host…".to_string()),
            ConnectionStatus::Disconnected => ("disconnected", "Offline".to_string()),
            ConnectionStatus::Error(msg) => ("error", format!("Error: {msg}")),
            ConnectionStatus::UpdateRequired {
                host_protocol,
                app_protocol,
                release_version,
            } => (
                "error",
                crate::state::update_required_message(
                    host_protocol,
                    app_protocol,
                    release_version.as_ref(),
                ),
            ),
            ConnectionStatus::NeedsAction { code, message } => {
                let prefix = if crate::state::needs_tyggs_sign_in(code) {
                    "Sign-in required"
                } else {
                    "Re-pair required"
                };
                ("error", format!("{prefix}: {message}"))
            }
        }
    };
    view! {
        {move || {
            let (class, text) = pill();
            view! {
                <span class={format!("status-pill {class}")}>{text}</span>
            }
        }}
    }
}

#[component]
fn ConnectDisconnectButton(
    state: AppState,
    local_host_id_for_connect: LocalHostId,
    local_host_id_for_disconnect: LocalHostId,
) -> impl IntoView {
    let id_status_lookup = local_host_id_for_connect.clone();
    let state_for_status = state.clone();
    let status = move || {
        state_for_status
            .connection_statuses
            .get()
            .get(&id_status_lookup)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    };
    let status_for_connected = status.clone();
    let is_connected = move || {
        matches!(
            status_for_connected(),
            ConnectionStatus::Connected
                | ConnectionStatus::Connecting
                | ConnectionStatus::Bootstrapping
        )
    };
    // A sticky incompatible-protocol reject can't be recovered by reconnecting
    // (the app-level handshake would just be rejected again), so the picker must
    // not offer a no-op Connect affordance for it.
    let status_for_update = status.clone();
    let needs_update =
        move || matches!(status_for_update(), ConnectionStatus::UpdateRequired { .. });
    // A managed pairing that stopped needing sign-in / re-pair: offer the exact
    // action instead of a no-op Connect (findings #3/#7).
    let status_for_action = status.clone();
    let managed_action = move || match status_for_action() {
        ConnectionStatus::NeedsAction { code, .. } => Some(crate::state::needs_tyggs_sign_in(code)),
        _ => None,
    };

    // Phase C MEDIUM: do NOT optimistically write Connecting/Error into
    // `connection_statuses` — that signal is owned by the
    // `tyde://paired-host-connection-status` event. The press-feedback for
    // an in-flight `connect_paired_host` invoke is tracked here only.
    let pending_invoke: RwSignal<Option<LocalHostId>> = RwSignal::new(None);

    let state_connect = state.clone();
    let on_connect = Callback::new(move |_: ()| {
        let id_typed = local_host_id_for_connect.clone();
        let state = state_connect.clone();
        pending_invoke.set(Some(id_typed.clone()));
        spawn_local(async move {
            let result = bridge::connect_paired_host(&id_typed).await;
            pending_invoke.set(None);
            if let Err(error) = result {
                log::error!("connect_paired_host({id_typed}) failed: {error}");
                // Surface via the global shell-error signal so the user sees
                // it; do not mutate `connection_statuses`.
                state
                    .mobile_shell_error
                    .set(Some(crate::state::MobileShellError {
                        code: protocol::MobileAccessErrorCode::Internal,
                        message: format!("connect failed: {error}"),
                    }));
            }
        });
    });

    let on_disconnect = Callback::new(move |_: ()| {
        let id_typed = local_host_id_for_disconnect.clone();
        spawn_local(async move {
            if let Err(error) = bridge::disconnect_paired_host(&id_typed).await {
                log::error!("disconnect_paired_host({id_typed}) failed: {error}");
            }
        });
    });

    // Re-pair: send the user back to the scanner to pair this host afresh through
    // tycode.dev. Forgetting is handled by the row's separate Delete button.
    let state_for_repair = state.clone();
    let on_repair = Callback::new(move |_: ()| {
        state_for_repair
            .app_mode
            .set(AppMode::Pairing(PairingScreen::Scanner));
    });
    let on_sign_in = Callback::new(move |provider: AuthProvider| {
        if let Err(error) = bridge::begin_tyggs_sign_in(provider, None) {
            log::error!("failed to start Tyggs sign-in: {error}");
        }
    });
    let _ = state;
    let _ = pending_invoke;

    view! {
        {move || if needs_update() {
            view! {
                <Button
                    label="Update required"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    disabled=true
                    data_mobile_test="paired-host-update-required"
                />
            }.into_any()
        } else if let Some(sign_in) = managed_action() {
            if sign_in {
                auth_provider_buttons(on_sign_in)
            } else {
                view! {
                    <Button
                        label="Re-pair"
                        variant=ButtonVariant::Primary
                        full_width=true
                        data_mobile_test="paired-host-repair"
                        on_click=on_repair
                    />
                }.into_any()
            }
        } else if is_connected() {
            view! {
                <Button
                    label="Disconnect"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    on_click=on_disconnect
                />
            }.into_any()
        } else {
            view! {
                <Button
                    label="Connect"
                    variant=ButtonVariant::Primary
                    full_width=true
                    on_click=on_connect
                />
            }.into_any()
        }}
    }
}

fn auth_provider_buttons(on_sign_in: Callback<AuthProvider>) -> AnyView {
    match bridge::tyggs_auth_providers() {
        Ok(providers) => view! {
            <div class="paired-host-provider-sign-in">
                {providers
                    .into_iter()
                    .enumerate()
                    .map(|(index, provider)| {
                        let test_id = if index == 0 {
                            "paired-host-sign-in"
                        } else {
                            paired_host_auth_provider_test_id(provider)
                        };
                        view! {
                            <button
                                type="button"
                                class="ui-button ui-button-primary ui-button-full"
                                data-mobile-test=test_id
                                data-mobile-auth-provider=provider.as_str()
                                aria-label=provider.sign_in_label()
                                on:click=move |_| on_sign_in.run(provider)
                            >
                                <span class="ui-button-label">{provider.sign_in_label()}</span>
                            </button>
                        }
                    })
                    .collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(message) => view! {
            <p class="pairing-error" role="alert" data-mobile-test="paired-host-provider-error">
                {message}
            </p>
        }
        .into_any(),
    }
}

fn paired_host_auth_provider_test_id(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::Apple => "paired-host-sign-in-apple",
        AuthProvider::Google => "paired-host-sign-in-google",
    }
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

    fn force_web_backend() {
        let window = web_sys::window().expect("window");
        let _ = js_sys::Reflect::delete_property(
            &window,
            &wasm_bindgen::JsValue::from_str("__TAURI__"),
        );
    }

    fn set_service_config(json: &str) {
        force_web_backend();
        let window = web_sys::window().expect("window");
        let key = wasm_bindgen::JsValue::from_str("__TYDE_MOBILE_SERVICE__");
        if json.is_empty() {
            let _ = js_sys::Reflect::delete_property(&window, &key);
            return;
        }
        let value = js_sys::JSON::parse(json).expect("parse service config");
        js_sys::Reflect::set(&window, &key, &value).expect("install service config");
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
    async fn renders_one_row_per_paired_host_and_reflects_status_changes() {
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.paired_hosts.set(vec![
                fixture_host("h1", "Laptop"),
                fixture_host("h2", "Desktop"),
            ]);
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Laptop"), "missing host label: {text}");
        assert!(text.contains("Desktop"), "missing host label: {text}");
        assert!(text.contains("Pair another host"));
        // Default status is Disconnected/Offline for both.
        assert!(
            text.matches("Offline").count() >= 2,
            "default status: {text}"
        );

        // Mutate connection_statuses for h1 and verify the pill reflects it.
        let state = state_handle.borrow().as_ref().unwrap().clone();
        state.connection_statuses.update(|m| {
            m.insert(LocalHostId("h1".to_owned()), ConnectionStatus::Connected);
        });
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Online"), "expected Online pill, got: {text}");
    }

    /// A host stuck in `UpdateRequired` must not offer a no-op Connect button;
    /// the picker shows a disabled "Update required" affordance instead.
    #[wasm_bindgen_test]
    async fn update_required_host_shows_disabled_update_button_not_connect() {
        let container = make_container();
        let host = LocalHostId("h-update".to_owned());
        let host_for_mount = host.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .paired_hosts
                .set(vec![fixture_host("h-update", "Studio")]);
            state.connection_statuses.update(|m| {
                m.insert(
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
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        let button: HtmlElement = container
            .query_selector("[data-mobile-test='paired-host-update-required']")
            .unwrap()
            .expect("update-required host must render the disabled update affordance")
            .dyn_into()
            .unwrap();
        assert!(
            button.has_attribute("disabled"),
            "the update-required affordance must be disabled, not a live no-op",
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Connect"),
            "no Connect button may be offered while an update is required: {text}"
        );
        // The status pill still names the host build so the user knows why.
        assert!(
            text.contains("0.8.19-beta.15"),
            "host build should surface: {text}"
        );
    }

    /// A managed host that needs a fresh Tyggs session offers an explicit
    /// "Sign in with Tyggs" action — not a no-op Connect (findings #3/#7).
    #[wasm_bindgen_test]
    async fn service_auth_required_host_offers_sign_in_not_connect() {
        force_web_backend();
        let container = make_container();
        let host = LocalHostId("h-auth".to_owned());
        let host_for_mount = host.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .paired_hosts
                .set(vec![fixture_host("h-auth", "Studio")]);
            state.connection_statuses.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::ServiceAuthRequired,
                        message: "Sign in with your Tyggs account again.".to_owned(),
                    },
                );
            });
            provide_context(state);
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='paired-host-sign-in']")
                .unwrap()
                .is_some(),
            "an auth-required host must offer a Tyggs sign-in action"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains(">Connect<") && text.contains("Sign-in required"),
            "no bare Connect; the pill must name the required action: {text}"
        );
    }

    #[wasm_bindgen_test]
    async fn service_auth_required_host_renders_provider_buttons_in_order() {
        set_service_config(r#"{"providers":["apple","google"]}"#);
        let container = make_container();
        let host = LocalHostId("h-auth-providers".to_owned());
        let host_for_mount = host.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .paired_hosts
                .set(vec![fixture_host("h-auth-providers", "Studio")]);
            state.connection_statuses.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::ServiceAuthRequired,
                        message: "Sign in with your Tyggs account again.".to_owned(),
                    },
                );
            });
            provide_context(state);
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        let legacy: HtmlElement = container
            .query_selector("[data-mobile-test='paired-host-sign-in']")
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
            Some("paired-host-sign-in-google")
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

    /// A managed host that needs re-pairing offers an explicit "Re-pair" action.
    #[wasm_bindgen_test]
    async fn repair_required_host_offers_repair_action() {
        let container = make_container();
        let host = LocalHostId("h-repair".to_owned());
        let host_for_mount = host.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .paired_hosts
                .set(vec![fixture_host("h-repair", "Studio")]);
            state.connection_statuses.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::NeedsAction {
                        code: protocol::MobileAccessErrorCode::RepairRequired,
                        message: "Re-pair from the host's current QR code.".to_owned(),
                    },
                );
            });
            provide_context(state);
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='paired-host-repair']")
                .unwrap()
                .is_some(),
            "a repair-required host must offer a Re-pair action"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Re-pair required"),
            "the pill must name the required action: {text}"
        );
    }

    #[wasm_bindgen_test]
    async fn delete_pairing_requires_second_tap() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .paired_hosts
                .set(vec![fixture_host("stale-host", "Stale Host")]);
            provide_context(state);
            view! { <PairedHostsPicker /> }
        });
        next_tick().await;

        let delete: HtmlElement = container
            .query_selector("[data-mobile-test='paired-host-forget']")
            .unwrap()
            .expect("host row must render a delete affordance")
            .dyn_into()
            .unwrap();
        delete.click();
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='paired-host-forget-confirm']")
                .unwrap()
                .is_some(),
            "first Delete tap must reveal the confirmation button"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='paired-host-forget-cancel']")
                .unwrap()
                .is_some(),
            "first Delete tap must reveal a cancel button"
        );
    }
}
