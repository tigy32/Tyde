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
                    >
                        <span class="mobile-shell-error-code">{code_label}</span>
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
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <MobileShellErrorBanner /> }
        });
        next_tick().await;

        let state = state_handle.borrow().as_ref().unwrap().clone();
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
    }
}
