use leptos::prelude::*;

use crate::state::AppState;

/// Top-of-app banner that surfaces `state.mobile_shell_error`. Renders in all
/// app modes (Onboarding/Workspace/Pairing) so a paste-failed-during-pairing
/// is visible. The user dismisses it with the close button; the same signal
/// is also written by listener-registration and `list_paired_hosts` failures
/// so those surface here too. (Phase C HIGH 4.)
#[component]
pub fn MobileShellErrorBanner() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    view! {
        {move || {
            state.mobile_shell_error.get().map(|err| {
                let code_label = format!("{:?}", err.code);
                let message = err.message.clone();
                let dismiss_state = state.clone();
                view! {
                    <div
                        class="mobile-shell-error-banner"
                        role="alert"
                        data-mobile-test="mobile-shell-error-banner"
                        data-error-code=code_label.clone()
                    >
                        {(!crate::native::is_native()).then(|| view! {
                            <span class="mobile-shell-error-code">{code_label.clone()}</span>
                        })}
                        <span class="mobile-shell-error-message">{message}</span>
                        <button
                            type="button"
                            class="error-banner-dismiss"
                            data-mobile-test="mobile-shell-error-dismiss"
                            aria-label="Dismiss error"
                            on:click=move |_| dismiss_state.mobile_shell_error.set(None)
                        >
                            "×"
                        </button>
                    </div>
                }
            })
        }}
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::MobileAccessErrorCode;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    use crate::state::MobileShellError;

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
    async fn renders_message_when_signal_is_set_and_disappears_when_cleared() {
        let container = make_container();
        // Each browser/native mount is disposable; the scenario state outlives all of them.
        let scenario = Owner::new();
        let state = scenario.with(AppState::new);
        let mounted_state = state.clone();
        let browser_handle = mount_to(container.clone(), move || {
            provide_context(mounted_state);
            view! { <MobileShellErrorBanner /> }
        });
        next_tick().await;

        // No error → empty banner.
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("invalid pairing"),
            "banner should be empty initially, got: {text}"
        );

        state.mobile_shell_error.set(Some(MobileShellError {
            code: MobileAccessErrorCode::InvalidPairingQr,
            message: "scan a real QR".to_owned(),
        }));
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("InvalidPairingQr"),
            "Browser error code presentation remains unchanged"
        );
        assert!(
            text.contains("scan a real QR"),
            "expected error message visible, got: {text}"
        );

        let close: HtmlElement = container
            .query_selector("[data-mobile-test='mobile-shell-error-dismiss']")
            .unwrap()
            .expect("dismiss button should render")
            .dyn_into()
            .unwrap();
        close.click();
        next_tick().await;
        assert!(
            state.mobile_shell_error.get_untracked().is_none(),
            "clicking dismiss should clear the shell error signal"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("scan a real QR"),
            "banner should hide after clicking dismiss, got: {text}"
        );

        state.mobile_shell_error.set(Some(MobileShellError {
            code: MobileAccessErrorCode::InvalidPairingQr,
            message: "scan a real QR".to_owned(),
        }));
        next_tick().await;

        // Clearing the signal removes the banner.
        state.mobile_shell_error.set(None);
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("scan a real QR"),
            "banner should hide after clear, got: {text}"
        );
        drop(browser_handle);
        container.remove();

        // Actual28 showed the same rejection as a shell alert, loader paragraph and connection paragraph.
        crate::components::test_styles::ensure_styles_loaded();
        let window = web_sys::window().unwrap();
        js_sys::Reflect::set(&window, &"__tydeNativeHost".into(), &js_sys::Object::new()).unwrap();
        let native = make_container();
        native.set_class_name("mobile-app");
        native.set_attribute("data-native-host", "").unwrap();
        native
            .style()
            .set_css_text("width:390px;height:800px;position:relative");
        let host = crate::state::LocalHostId("native-error-host".into());
        state.active_local_host_id.set(Some(host.clone()));
        state.app_mode.set(crate::state::AppMode::Workspace);
        let mismatch = crate::state::ConnectionStatus::UpdateRequired {
            host_protocol: protocol::PROTOCOL_VERSION,
            app_protocol: protocol::PROTOCOL_VERSION,
            release_version: Some(protocol::TydeReleaseVersion::parse("0.9.6-beta.15").unwrap()),
        };
        state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), mismatch.clone());
        });
        state
            .bundle_status
            .set(crate::bundle::BundleStatus::Unavailable(
                crate::bridge::loader::UnavailableReason::NativeVersion,
            ));
        state.mobile_shell_error.set(Some(MobileShellError {
            code: MobileAccessErrorCode::VersionMismatch,
            message: "This app and host use different releases. Update the app and host together."
                .into(),
        }));
        let mounted_state = state.clone();
        let native_handle = mount_to(native.clone(), move || {
            provide_context(mounted_state);
            view! { <MobileShellErrorBanner /><crate::components::ConnectionBanner /><crate::components::HomeView /> }
        });
        let result: Result<(), String> = async {
            let check = |condition: bool, message: &str| if condition { Ok(()) } else { Err(message.to_owned()) };
            let visible = |selector: &str| {
                let nodes = native.query_selector_all(selector).unwrap();
                (0..nodes.length()).filter(|i| {
                    nodes.item(*i).unwrap().dyn_into::<web_sys::Element>().unwrap().get_bounding_client_rect().height() > 0.0
                }).count()
            };
            next_tick().await;
            for width in [320, 390, 834, 1210] {
                native.style().set_property("width", &format!("{width}px")).unwrap();
                next_tick().await;
                check(visible("[role='alert']") == 1, "Native mismatch must have one visible error, not three parallel diagnostic paragraphs")?;
                check(native.query_selector("[data-mobile-test='home-connection-pill']").unwrap().is_none()
                    && native.query_selector("[data-mobile-test='home-stat-host']").unwrap().is_none(),
                    "Native Home keeps one honest dot/status rather than repeated pill and connection card")?;
                check(native.query_selector(".home-hero-status-label").unwrap().unwrap().text_content().as_deref() == Some("Update required"),
                    "Reducing duplicate notices must preserve the actual typed update-required status")?;
            }
            native.query_selector("[data-mobile-test='mobile-shell-error-dismiss']").unwrap().unwrap().dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(visible("[role='alert']") == 1 && visible("[data-mobile-test='connection-banner-update-required']") == 1,
                "Dismissing a shell notice must retain the single canonical sticky compatibility error")?;
            check(state.connection_statuses.get_untracked().get(&host) == Some(&mismatch),
                "Notice dismissal must never clear or change the compatibility verdict")?;
            check(native.query_selector("[data-mobile-test='connection-banner-update-required']").unwrap().unwrap().text_content().unwrap().contains("Update this app and host together to connect."),
                "The native compatibility notice must be humane rather than a technical build/protocol paragraph")?;
            state.command_errors_by_host.update(|errors| { errors.insert(host.clone(), "The requested chat could not be created.".into()); });
            next_tick().await;
            check(visible("[role='alert']") == 1 && visible(".command-error-banner") == 1,
                "An actionable command failure must not stack with another connection paragraph")?;
            state.command_errors_by_host.update(|errors| { errors.remove(&host); });
            state.connection_statuses.update(|statuses| { statuses.insert(host.clone(), crate::state::ConnectionStatus::NeedsAction {
                code: MobileAccessErrorCode::RepairRequired, message: "Pair with this host again.".into(),
            }); });
            state.mobile_shell_error.set(Some(MobileShellError { code: MobileAccessErrorCode::KeystoreFailed, message: "Native secure storage operation was denied.".into() }));
            next_tick().await;
            check(visible("[role='alert']") == 1 && visible("[data-mobile-test='connection-banner-repair']") == 0,
                "A global error must not invent recovery ownership from the currently selected host")?;
            native.query_selector("[data-mobile-test='mobile-shell-error-dismiss']").unwrap().unwrap().dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            let repair = native.query_selector("[data-mobile-test='connection-banner-repair']").unwrap().ok_or("Dismissing the global notice must expose the selected host's real typed recovery")?;
            check(repair.get_bounding_client_rect().height() >= 44.0, "Native error recovery must keep a usable touch target")?;
            repair.dyn_into::<HtmlElement>().unwrap().click();
            next_tick().await;
            check(state.app_mode.get_untracked() == crate::state::AppMode::Pairing(crate::state::PairingScreen::Scanner),
                "Re-pair must still open the ordinary scanner route, not a debug or reset path")?;
            state.app_mode.set(crate::state::AppMode::Workspace);
            state.connection_statuses.update(|statuses| { statuses.insert(host.clone(), crate::state::ConnectionStatus::Disconnected); });
            next_tick().await;
            check(visible("[data-mobile-test='connection-banner-reconnect']") == 1,
                "The selected disconnected host retains its normal Reconnect action")?;
            state.app_mode.set(crate::state::AppMode::HostPicker);
            let unrelated = crate::state::LocalHostId("unregistered-native-error-host-b".into());
            state.paired_hosts.set(vec![host.clone(), unrelated.clone()].into_iter().map(|id| crate::state::PairedHostSummary {
                host_label: if id == host { "Selected host".into() } else { "Other host".into() },
                local_host_id: id, direct_origin: None, credential_fingerprint: "public-test-fingerprint".into(),
                auto_connect: false, last_connected_at_ms: None,
            }).collect());
            let chooser = make_container();
            chooser.set_class_name("mobile-app");
            chooser.set_attribute("data-native-host", "").unwrap();
            let mounted_state = state.clone();
            let chooser_handle = mount_to(chooser.clone(), move || {
                provide_context(mounted_state);
                view! { <MobileShellErrorBanner /><crate::components::PairedHostsPicker /> }
            });
            next_tick().await;
            // Production App subscribes before Connect; this partial mount must
            // project that same manager event instead of expecting a shell error.
            let observed = state.clone();
            let status_listener = crate::bridge::listen_paired_host_connection_status(move |event| {
                crate::app::apply_connection_status(&observed, event.local_host_id, event.status, event.connection_instance_id);
            }).await.unwrap();
            let connect = chooser.query_selector_all(".paired-host-card").unwrap().item(1).unwrap()
                .dyn_into::<web_sys::Element>().unwrap().query_selector("button").unwrap().unwrap();
            let finished = js_sys::Function::new_with_args("root,button", r#"
                return new Promise(resolve => {
                    // The captured DOM already contained B's exact failure;
                    // Leptos updated the text node without replacing children.
                    const settled = () => {
                        if (root.textContent.includes('This saved host is unavailable.')) {
                            observer.disconnect(); resolve();
                        }
                    };
                    const observer = new MutationObserver(settled);
                    observer.observe(root, {childList:true, subtree:true, characterData:true});
                    button.click(); settled();
                });
            "#).call2(&wasm_bindgen::JsValue::NULL, &chooser, &connect).unwrap();
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(finished)).await.unwrap();
            next_tick().await;
            let failed_for_other = state.mobile_shell_error.get_untracked().is_none()
                && matches!(state.connection_statuses.get_untracked().get(&unrelated), Some(crate::state::ConnectionStatus::Error(message)) if message.starts_with("This saved host is unavailable."));
            let selection_unchanged = state.active_local_host_id.get_untracked().as_ref() == Some(&host);
            let no_wrong_recovery = chooser.query_selector(".mobile-shell-error-banner [data-mobile-test='connection-banner-reconnect'],.mobile-shell-error-banner [data-mobile-test='connection-banner-repair']").unwrap().is_none();
            let can_retry_own_row = chooser.query_selector_all(".paired-host-card").unwrap().item(1).unwrap()
                .dyn_into::<web_sys::Element>().unwrap().query_selector("button").unwrap()
                .is_some_and(|button| !button.has_attribute("disabled"));
            status_listener.remove();
            drop(chooser_handle);
            chooser.remove();
            check(failed_for_other && selection_unchanged && no_wrong_recovery && can_retry_own_row,
                "A real failed Connect for nonselected B must not reconnect A or offer an unowned shell recovery; B's own row stays actionable")?;
            Ok(())
        }.await;
        drop(native_handle);
        native.remove();
        js_sys::Reflect::delete_property(&window, &"__tydeNativeHost".into()).unwrap();
        assert!(result.is_ok(), "{}", result.unwrap_err());
        state.connection_statuses.update(|statuses| {
            statuses.insert(host, mismatch);
        });
        state.mobile_shell_error.set(Some(MobileShellError {
            code: MobileAccessErrorCode::KeystoreFailed,
            message: "Native secure storage operation was denied.".into(),
        }));
        let browser = make_container();
        let mounted_state = state.clone();
        let browser_handle = mount_to(browser.clone(), move || {
            provide_context(mounted_state);
            view! { <MobileShellErrorBanner /><crate::components::ConnectionBanner /><crate::components::HomeView /> }
        });
        next_tick().await;
        assert!(
            browser.text_content().unwrap().contains("KeystoreFailed"),
            "Browser shell code presentation is unchanged"
        );
        assert!(
            browser
                .query_selector("[data-mobile-test='bundle-sync']")
                .unwrap()
                .is_some(),
            "Browser retains its own executable loader notice"
        );
        assert!(
            browser
                .query_selector("[data-mobile-test='connection-banner-update-required']")
                .unwrap()
                .is_some(),
            "Browser protocol details remain visible"
        );
        assert!(
            browser
                .query_selector("[data-mobile-test='home-connection-pill']")
                .unwrap()
                .is_some()
                && browser
                    .query_selector("[data-mobile-test='home-stat-host']")
                    .unwrap()
                    .is_some(),
            "Browser Home layout remains unchanged"
        );
        drop(browser_handle);
        browser.remove();
        scenario.cleanup();
    }
}
